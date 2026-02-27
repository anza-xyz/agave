use {
    crate::{
        nonblocking::{
            load_debt_tracker::LoadDebtTracker,
            qos::{ConnectionContext, OpaqueStreamerCounter, ParkedStreamMode, QosController},
            quic::{
                get_connection_stake, update_open_connections_stat, ClientConnectionTracker,
                ConnectionHandlerError, ConnectionPeerType, ConnectionTable, ConnectionTableKey,
                ConnectionTableType, CONNECTION_CLOSE_CODE_DISALLOWED,
                CONNECTION_CLOSE_REASON_DISALLOWED,
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
    quinn::Connection,
    solana_time_utils as timing,
    std::{
        collections::HashMap,
        future::Future,
        sync::{
            atomic::{AtomicU64, AtomicUsize, Ordering},
            Arc, RwLock,
        },
        time::Duration,
    },
    tokio::sync::{Mutex, MutexGuard},
    tokio_util::sync::CancellationToken,
};

/// Reference RTT for BDP scaling
const REFERENCE_RTT: Duration = Duration::from_millis(100);

/// Max RTT for BDP clamping
const MAX_RTT: Duration = Duration::from_millis(200);

/// Min RTT floor
const MIN_RTT: Duration = Duration::from_millis(1);

/// Base max concurrent streams at reference RTT when system is not saturated
const DEFAULT_BASE_MAX_STREAMS: u32 = 2048;

/// Base max concurrent streams for unstaked peers at reference RTT.
/// Matches the old stream_throttle effective limit of 200 TPS * 100ms = 20.
const DEFAULT_BASE_MAX_STREAMS_UNSTAKED: u32 = 20;

/// Per-key counter shared by all connections from the same peer (pubkey or IP).
/// Tracks how many connections currently exist for the key, so that
/// compute_max_streams can divide the quota evenly — removing any incentive
/// to open multiple connections.
pub(crate) struct SwQosStreamerCounter {
    connection_count: AtomicUsize,
}
impl OpaqueStreamerCounter for SwQosStreamerCounter {}

#[derive(Clone)]
pub struct SwQosConfig {
    pub max_streams_per_ms: u64,
    pub max_staked_connections: usize,
    pub max_unstaked_connections: usize,
    pub max_connections_per_staked_peer: usize,
    pub max_connections_per_unstaked_peer: usize,
    pub base_max_streams: u32,
    pub base_max_streams_unstaked: u32,
    /// Low-water threshold (in tokens) for saturation detection. If None, defaults
    /// to 10% of the burst capacity.
    pub saturation_threshold_tokens: Option<u64>,
    pub parked_stream_mode: ParkedStreamMode,
    /// Per-peer RTT overrides for quota calculation (testing only).
    /// When a peer's pubkey is in the map, `compute_max_streams` uses
    /// the mapped duration instead of `connection.rtt()`.
    pub rtt_overrides: HashMap<solana_pubkey::Pubkey, Duration>,
}

impl Default for SwQosConfig {
    fn default() -> Self {
        SwQosConfig {
            max_streams_per_ms: DEFAULT_MAX_STREAMS_PER_MS,
            max_staked_connections: DEFAULT_MAX_STAKED_CONNECTIONS,
            max_unstaked_connections: DEFAULT_MAX_UNSTAKED_CONNECTIONS,
            max_connections_per_staked_peer: DEFAULT_MAX_QUIC_CONNECTIONS_PER_STAKED_PEER,
            max_connections_per_unstaked_peer: DEFAULT_MAX_QUIC_CONNECTIONS_PER_UNSTAKED_PEER,
            base_max_streams: DEFAULT_BASE_MAX_STREAMS,
            base_max_streams_unstaked: DEFAULT_BASE_MAX_STREAMS_UNSTAKED,
            saturation_threshold_tokens: None,
            parked_stream_mode: ParkedStreamMode::Park,
            rtt_overrides: HashMap::new(),
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
    capacity_tps: u64,
    load_tracker: Arc<LoadDebtTracker>,
    stats: Arc<StreamerStats>,
    staked_nodes: Arc<RwLock<StakedNodes>>,
    unstaked_connection_table: Arc<Mutex<ConnectionTable<SwQosStreamerCounter>>>,
    staked_connection_table: Arc<Mutex<ConnectionTable<SwQosStreamerCounter>>>,
}

#[derive(Clone)]
pub struct SwQosConnectionContext {
    peer_type: ConnectionPeerType,
    remote_pubkey: Option<solana_pubkey::Pubkey>,
    total_stake: u64,
    in_staked_table: bool,
    last_update: Arc<AtomicU64>,
    stream_counter: Option<Arc<SwQosStreamerCounter>>,
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
    pub fn load_tracker(&self) -> &LoadDebtTracker {
        &self.load_tracker
    }

    pub fn new(
        config: SwQosConfig,
        stats: Arc<StreamerStats>,
        staked_nodes: Arc<RwLock<StakedNodes>>,
        cancel: CancellationToken,
    ) -> Self {
        let max_streams_per_second = config.max_streams_per_ms * 1000;
        let burst_capacity = max_streams_per_second / 10;
        let saturation_threshold_tokens = config
            .saturation_threshold_tokens
            .unwrap_or(burst_capacity / 10);

        Self {
            config,
            capacity_tps: max_streams_per_second,
            load_tracker: Arc::new(LoadDebtTracker::new(
                max_streams_per_second,
                burst_capacity,
                Duration::from_millis(2),
                saturation_threshold_tokens,
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

    /// Core MAX_STREAMS computation, separated from `compute_max_streams`
    /// so it can be called directly in unit tests (without a quinn::Connection).
    pub(crate) fn compute_max_streams_for_rtt(
        &self,
        context: &SwQosConnectionContext,
        rtt: Duration,
        saturated: bool,
    ) -> Option<u32> {
        let rtt = rtt.clamp(MIN_RTT, MAX_RTT);

        // BDP-scaled generous credit: at REFERENCE_RTT → base_max_streams,
        // scales linearly with RTT.
        let rtt_scale = rtt.as_secs_f64() / REFERENCE_RTT.as_secs_f64();
        let staked_unsat_max = (self.config.base_max_streams as f64 * rtt_scale) as u32;
        let unstaked_unsat_max = (self.config.base_max_streams_unstaked as f64 * rtt_scale) as u32;

        if saturated {
            match context.peer_type {
                ConnectionPeerType::Unstaked => Some(0), // park
                ConnectionPeerType::Staked(stake) => {
                    // Use u128 to avoid saturating_mul overflow for large lamport
                    // stakes: capacity_tps (u64) * stake (u64) can exceed u64::MAX
                    // for any validator above ~37K SOL, silently collapsing all
                    // large stakers to the same share_tps and destroying
                    // proportionality.  The result fits back in u64 because
                    // share_tps ≤ capacity_tps (stake ≤ total_stake).
                    let share_tps = (self.capacity_tps as u128)
                        .saturating_mul(stake as u128)
                        .checked_div(context.total_stake as u128)
                        .unwrap_or(0) as u64;
                    let quota = (share_tps as f64 * rtt.as_secs_f64()) as u32;
                    let num_connections = context
                        .stream_counter
                        .as_ref()
                        .map(|c| c.connection_count.load(Ordering::Relaxed))
                        .unwrap_or(1)
                        .max(1) as u32;
                    let per_conn = (quota / num_connections).max(1);
                    // Cap at the unsaturated limit: a connection should not get
                    // more quota under load than when the system is idle.
                    Some(per_conn.min(staked_unsat_max.max(1)))
                }
            }
        } else {
            match context.peer_type {
                ConnectionPeerType::Unstaked => Some(unstaked_unsat_max.max(1)),
                ConnectionPeerType::Staked(_) => Some(staked_unsat_max.max(1)),
            }
        }
    }
}

impl SwQos {
    fn cache_new_connection(
        &self,
        client_connection_tracker: ClientConnectionTracker,
        connection: &Connection,
        mut connection_table_l: MutexGuard<ConnectionTable<SwQosStreamerCounter>>,
        conn_context: &SwQosConnectionContext,
    ) -> Result<
        (Arc<AtomicU64>, CancellationToken, Arc<SwQosStreamerCounter>),
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
                    Arc::new(SwQosStreamerCounter {
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
        unstaked_connection_table: &mut ConnectionTable<SwQosStreamerCounter>,
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
        connection_table: Arc<Mutex<ConnectionTable<SwQosStreamerCounter>>>,
        max_connections: usize,
        conn_context: &SwQosConnectionContext,
    ) -> Result<
        (Arc<AtomicU64>, CancellationToken, Arc<SwQosStreamerCounter>),
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

impl QosController<SwQosConnectionContext> for SwQos {
    fn build_connection_context(&self, connection: &Connection) -> SwQosConnectionContext {
        get_connection_stake(connection, &self.staked_nodes).map_or(
            SwQosConnectionContext {
                peer_type: ConnectionPeerType::Unstaked,
                total_stake: 0,
                remote_pubkey: None,
                in_staked_table: false,
                last_update: Arc::new(AtomicU64::new(timing::timestamp())),
                stream_counter: None,
            },
            |(pubkey, stake, total_stake)| {
                // Demote ultra-low-stake peers to unstaked. The threshold
                // mirrors the old stream_throttle heuristic: stake must be
                // large enough to earn at least 1 stream per 100ms throttle
                // interval at full capacity.
                let min_stake_ratio = 1_f64 / (self.config.max_streams_per_ms * 100) as f64;
                let stake_ratio = stake as f64 / total_stake as f64;
                let peer_type = if stake == 0 || stake_ratio < min_stake_ratio {
                    ConnectionPeerType::Unstaked
                } else {
                    ConnectionPeerType::Staked(stake)
                };

                SwQosConnectionContext {
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

    fn is_saturated(&self) -> bool {
        self.load_tracker.is_saturated()
    }

    fn compute_max_streams(
        &self,
        context: &SwQosConnectionContext,
        connection: &Connection,
        saturated: bool,
    ) -> Option<u32> {
        let rtt = context
            .remote_pubkey
            .and_then(|pk| self.config.rtt_overrides.get(&pk).copied())
            .unwrap_or_else(|| connection.rtt());
        self.compute_max_streams_for_rtt(context, rtt, saturated)
    }

    fn on_stream_accepted(&self, _context: &SwQosConnectionContext) {
        self.load_tracker.acquire();
    }

    fn on_stream_error(&self, _conn_context: &SwQosConnectionContext) {}

    fn on_stream_closed(&self, _conn_context: &SwQosConnectionContext) {}

    #[allow(clippy::manual_async_fn)]
    fn remove_connection(
        &self,
        conn_context: &SwQosConnectionContext,
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

    fn on_stream_finished(&self, context: &SwQosConnectionContext) {
        context
            .last_update
            .store(timing::timestamp(), Ordering::Relaxed);
    }

    #[allow(clippy::manual_async_fn)]
    fn on_new_stream(&self, _context: &SwQosConnectionContext) -> impl Future<Output = ()> + Send {
        async {}
    }

    fn parked_stream_mode(&self, context: &SwQosConnectionContext) -> ParkedStreamMode {
        if context.peer_type().is_staked() {
            ParkedStreamMode::Allow
        } else {
            self.config.parked_stream_mode
        }
    }

    fn pull_stats(&self, stats: &crate::quic::StreamerStats, elapsed: Duration) {
        stats.pull_saturation_stats(&self.load_tracker, elapsed);
    }

    fn max_concurrent_connections(&self) -> usize {
        // Allow 25% more connections than required to allow for handshake
        (self.config.max_staked_connections + self.config.max_unstaked_connections) * 5 / 4
    }
}

#[cfg(test)]
pub mod test {
    use super::*;

    fn make_swqos(config: SwQosConfig) -> SwQos {
        let cancel = CancellationToken::new();
        let stats = Arc::new(StreamerStats::default());
        let staked_nodes = Arc::new(RwLock::new(crate::streamer::StakedNodes::default()));
        SwQos::new(config, stats, staked_nodes, cancel)
    }

    fn unstaked_context() -> SwQosConnectionContext {
        SwQosConnectionContext {
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
    ) -> SwQosConnectionContext {
        let counter = Arc::new(SwQosStreamerCounter {
            connection_count: AtomicUsize::new(num_connections),
        });
        SwQosConnectionContext {
            peer_type: ConnectionPeerType::Staked(stake),
            remote_pubkey: None,
            total_stake,
            in_staked_table: true,
            last_update: Arc::new(AtomicU64::new(0)),
            stream_counter: Some(counter),
        }
    }

    // ── Saturated path ──────────────────────────────────────────────

    #[test]
    fn test_saturated_unstaked_returns_zero() {
        let swqos = make_swqos(SwQosConfig::default());
        let ctx = unstaked_context();
        assert_eq!(
            swqos.compute_max_streams_for_rtt(&ctx, Duration::from_millis(50), true),
            Some(0),
        );
    }

    #[test]
    fn test_saturated_staked_proportional_quota() {
        // 500K/s capacity, 1% stake, 50ms RTT → 5000 * 0.05 = 250
        let swqos = make_swqos(SwQosConfig {
            max_streams_per_ms: 500,
            ..SwQosConfig::default()
        });
        let ctx = staked_context(1_000, 100_000, 1);
        assert_eq!(
            swqos.compute_max_streams_for_rtt(&ctx, Duration::from_millis(50), true),
            Some(250),
        );
    }

    #[test]
    fn test_saturated_quota_scales_with_rtt() {
        // Same stake, double RTT → double quota (throughput stays the same)
        let swqos = make_swqos(SwQosConfig {
            max_streams_per_ms: 500,
            ..SwQosConfig::default()
        });
        let ctx = staked_context(1_000, 100_000, 1);
        let q50 = swqos.compute_max_streams_for_rtt(&ctx, Duration::from_millis(50), true);
        let q100 = swqos.compute_max_streams_for_rtt(&ctx, Duration::from_millis(100), true);
        assert_eq!(q50, Some(250));
        assert_eq!(q100, Some(500));
    }

    #[test]
    fn test_saturated_quota_divided_by_connections() {
        let swqos = make_swqos(SwQosConfig {
            max_streams_per_ms: 500,
            ..SwQosConfig::default()
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
        let swqos = make_swqos(SwQosConfig {
            max_streams_per_ms: 500,
            ..SwQosConfig::default()
        });
        let ctx = staked_context(1, 1_000_000_000, 1);
        assert_eq!(
            swqos.compute_max_streams_for_rtt(&ctx, Duration::from_millis(50), true),
            Some(1),
        );
    }

    #[test]
    fn test_saturated_total_stake_zero_no_panic() {
        let swqos = make_swqos(SwQosConfig {
            max_streams_per_ms: 500,
            ..SwQosConfig::default()
        });
        let ctx = staked_context(1_000, 0, 1);
        // checked_div(0) → unwrap_or(0) → quota=0 → .max(1) → 1
        assert_eq!(
            swqos.compute_max_streams_for_rtt(&ctx, Duration::from_millis(50), true),
            Some(1),
        );
    }

    // ── Unsaturated path ────────────────────────────────────────────

    #[test]
    fn test_unsaturated_base_at_reference_rtt() {
        let swqos = make_swqos(SwQosConfig {
            base_max_streams: 2048,
            ..SwQosConfig::default()
        });
        let ctx = staked_context(1_000, 100_000, 1);
        // At REFERENCE_RTT (100ms): rtt_scale=1.0 → base_max_streams
        assert_eq!(
            swqos.compute_max_streams_for_rtt(&ctx, REFERENCE_RTT, false),
            Some(2048),
        );
    }

    #[test]
    fn test_unsaturated_scales_linearly_with_rtt() {
        let swqos = make_swqos(SwQosConfig {
            base_max_streams: 2048,
            ..SwQosConfig::default()
        });
        let ctx = staked_context(1_000, 100_000, 1);
        let q100 = swqos
            .compute_max_streams_for_rtt(&ctx, Duration::from_millis(100), false)
            .unwrap();
        let q200 = swqos
            .compute_max_streams_for_rtt(&ctx, Duration::from_millis(200), false)
            .unwrap();
        // 100ms = 1x ref → ~2048, 200ms = 2x ref → ~4096
        assert!((q100 as i32 - 2048).abs() <= 1, "q100={q100}");
        assert!((q200 as i32 - 4096).abs() <= 1, "q200={q200}");
        // Ratio should be 2x
        assert!((q200 as f64 / q100 as f64 - 2.0).abs() < 0.01);
    }

    #[test]
    fn test_unsaturated_low_rtt_scales_down() {
        let swqos = make_swqos(SwQosConfig {
            base_max_streams: 2048,
            ..SwQosConfig::default()
        });
        let ctx = staked_context(1_000, 100_000, 1);
        // RTT < REFERENCE_RTT scales down (not clamped, 5ms is above MIN_RTT).
        // 2048 * 5/100 = 102.4 → 102
        assert_eq!(
            swqos.compute_max_streams_for_rtt(&ctx, Duration::from_millis(5), false),
            Some(102),
        );
    }

    #[test]
    fn test_unsaturated_rtt_clamped_at_max() {
        let swqos = make_swqos(SwQosConfig {
            base_max_streams: 2048,
            ..SwQosConfig::default()
        });
        let ctx = staked_context(1_000, 100_000, 1);
        // 500ms RTT gets clamped to MAX_RTT (200ms) → scale = 2.0 → 4096
        let q_500 = swqos.compute_max_streams_for_rtt(&ctx, Duration::from_millis(500), false);
        let q_max = swqos.compute_max_streams_for_rtt(&ctx, MAX_RTT, false);
        assert_eq!(q_500, q_max);
        assert_eq!(q_max, Some(4096));
    }

    #[test]
    fn test_unsaturated_ignores_stake() {
        // In unsaturated mode, quota depends only on RTT, not stake
        let swqos = make_swqos(SwQosConfig {
            base_max_streams: 2048,
            ..SwQosConfig::default()
        });
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
        let swqos = make_swqos(SwQosConfig {
            base_max_streams_unstaked: 128,
            ..SwQosConfig::default()
        });
        let ctx = unstaked_context();
        // At REFERENCE_RTT (100ms): rtt_scale=1.0 → base_max_streams_unstaked
        assert_eq!(
            swqos.compute_max_streams_for_rtt(&ctx, REFERENCE_RTT, false),
            Some(128),
        );
    }

    // ── Hard cap ─────────────────────────────────────────────────────

    #[test]
    fn test_saturated_quota_capped_by_unsaturated_max() {
        // 100% stake at 50ms RTT: proportional quota = 500K * 0.05 = 25000,
        // capped at staked_unsat_max = base_max_streams * 0.5 = 1024.
        // A connection should not get more quota under load than when idle.
        let swqos = make_swqos(SwQosConfig {
            max_streams_per_ms: 500,
            base_max_streams: 2048,
            ..SwQosConfig::default()
        });
        let ctx = staked_context(1_000_000, 1_000_000, 1);
        assert_eq!(
            swqos.compute_max_streams_for_rtt(&ctx, Duration::from_millis(50), true),
            Some(1024),
        );
    }

    #[test]
    fn test_saturated_quota_proportional_small_stake() {
        // 1% stake at 50ms RTT: quota = 5000 * 0.05 = 250 (unchanged).
        let swqos = make_swqos(SwQosConfig {
            max_streams_per_ms: 500,
            base_max_streams: 2048,
            ..SwQosConfig::default()
        });
        let ctx = staked_context(1_000, 100_000, 1);
        assert_eq!(
            swqos.compute_max_streams_for_rtt(&ctx, Duration::from_millis(50), true),
            Some(250),
        );
    }

    #[test]
    fn test_saturated_large_lamport_stakes_preserve_proportionality() {
        // Regression test for u64 saturating_mul overflow.
        // Without u128, capacity_tps * stake overflows for any validator above
        // ~37K SOL, collapsing all large stakers to the same share_tps and
        // making a 10M-SOL staker appear identical to (or worse than) a 10K-SOL
        // staker with multiple connections.
        let swqos = make_swqos(SwQosConfig {
            max_streams_per_ms: 500,
            ..SwQosConfig::default()
        });
        const LAMPORTS_PER_SOL: u64 = 1_000_000_000;
        let stake_large = 10_000_000 * LAMPORTS_PER_SOL; // 10M SOL
        let stake_small = 10_000 * LAMPORTS_PER_SOL; // 10K SOL
        let total = stake_large + stake_small;
        let rtt = Duration::from_millis(100);

        let ctx_large = staked_context(stake_large, total, 1);
        let ctx_small = staked_context(stake_small, total, 1);

        // 500K TPS * (10M / 10.01M) * 100ms ≈ 49_950 → capped at staked_unsat_max (2048)
        // 500K TPS * (10K / 10.01M) * 100ms ≈ 49           → below cap
        //
        // Without the u128 fix the 10M-SOL share_tps overflows to
        // u64::MAX / total_stake ≈ 1_836, giving quota ≈ 183 — well below the
        // 2048 cap and barely 3-4x the 10K-SOL staker instead of ~1000x.
        let q_large = swqos
            .compute_max_streams_for_rtt(&ctx_large, rtt, true)
            .unwrap();
        let q_small = swqos
            .compute_max_streams_for_rtt(&ctx_small, rtt, true)
            .unwrap();
        // Large staker correctly hits the unsaturated cap, proving share_tps
        // was computed without overflow.
        assert_eq!(q_large, 2048);
        assert_eq!(q_small, 49);
        assert!(
            q_large > q_small * 40,
            "large/small ratio={}, expected large staker to dominate",
            q_large / q_small
        );
    }

    // ── Parked stream mode ───────────────────────────────────────────

    #[test]
    fn test_parked_stream_mode_staked_returns_allow() {
        let swqos = make_swqos(SwQosConfig::default());
        let ctx = staked_context(1_000, 100_000, 1);
        assert_eq!(swqos.parked_stream_mode(&ctx), ParkedStreamMode::Allow);
    }

    #[test]
    fn test_parked_stream_mode_unstaked_returns_config_default() {
        let swqos = make_swqos(SwQosConfig::default());
        let ctx = unstaked_context();
        assert_eq!(swqos.parked_stream_mode(&ctx), ParkedStreamMode::Park);
    }

    #[test]
    fn test_parked_stream_mode_unstaked_returns_config_override() {
        let swqos = make_swqos(SwQosConfig {
            parked_stream_mode: ParkedStreamMode::Reset,
            ..SwQosConfig::default()
        });
        let ctx = unstaked_context();
        assert_eq!(swqos.parked_stream_mode(&ctx), ParkedStreamMode::Reset);
    }
}
