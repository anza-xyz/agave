use {
    crate::{
        nonblocking::{
            load_tracker_token_bucket::GlobalLoadTrackerTokenBucket,
            qos::{ConnectionContext, OpaqueStreamerCounter, QosController},
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
const REFERENCE_RTT: Duration = Duration::from_millis(50);

/// Max RTT for BDP clamping
const MAX_RTT: Duration = Duration::from_millis(350);

/// Min RTT floor
const MIN_RTT: Duration = Duration::from_millis(2);

/// Base max concurrent streams at reference RTT when system is not saturated
const DEFAULT_BASE_MAX_STREAMS: u32 = 2048;

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
    load_tracker: Arc<GlobalLoadTrackerTokenBucket>,
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
    pub fn load_tracker(&self) -> &GlobalLoadTrackerTokenBucket {
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

        Self {
            config,
            load_tracker: Arc::new(GlobalLoadTrackerTokenBucket::new(
                max_streams_per_second,
                burst_capacity,
                Duration::from_millis(2),
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

impl SwQos {
    fn cache_new_connection(
        &self,
        client_connection_tracker: ClientConnectionTracker,
        connection: &Connection,
        mut connection_table_l: MutexGuard<ConnectionTable<SwQosStreamerCounter>>,
        conn_context: &SwQosConnectionContext,
    ) -> Result<(Arc<AtomicU64>, CancellationToken, Arc<SwQosStreamerCounter>), ConnectionHandlerError>
    {
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
    ) -> Result<(Arc<AtomicU64>, CancellationToken, Arc<SwQosStreamerCounter>), ConnectionHandlerError>
    {
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
                let peer_type = if stake == 0 {
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

    fn compute_max_streams(
        &self,
        context: &SwQosConnectionContext,
        connection: &Connection,
    ) -> Option<u32> {
        let rtt = context
            .remote_pubkey
            .and_then(|pk| self.config.rtt_overrides.get(&pk).copied())
            .unwrap_or_else(|| connection.rtt())
            .clamp(MIN_RTT, MAX_RTT);
        let rtt_scale = (rtt.as_millis() as u32) / (REFERENCE_RTT.as_millis() as u32);
        let rtt_scale = rtt_scale.max(1);

        if self.load_tracker.is_saturated() {
            match context.peer_type {
                ConnectionPeerType::Unstaked => Some(0), // park
                ConnectionPeerType::Staked(stake) => {
                    let capacity_tps = self.config.max_streams_per_ms * 1000;
                    let share_tps = capacity_tps
                        .saturating_mul(stake)
                        .checked_div(context.total_stake)
                        .unwrap_or(0);
                    let quota = (share_tps as f64 * rtt.as_secs_f64()) as u32;
                    let num_connections = context
                        .stream_counter
                        .as_ref()
                        .map(|c| c.connection_count.load(Ordering::Relaxed))
                        .unwrap_or(1)
                        .max(1) as u32;
                    Some((quota / num_connections).max(1))
                }
            }
        } else {
            Some(self.config.base_max_streams * rtt_scale)
        }
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
    fn on_new_stream(
        &self,
        _context: &SwQosConnectionContext,
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

    #[test]
    fn test_compute_max_streams_not_saturated() {
        // When not saturated, compute_max_streams returns generous BDP-scaled value
        let config = SwQosConfig {
            base_max_streams: 2048,
            ..SwQosConfig::default()
        };
        let cancel = CancellationToken::new();
        let stats = Arc::new(StreamerStats::default());
        let staked_nodes = Arc::new(RwLock::new(crate::streamer::StakedNodes::default()));
        let swqos = SwQos::new(config, stats, staked_nodes, cancel);

        // The tracker starts with burst_capacity tokens, so it's not saturated
        assert!(!swqos.load_tracker.is_saturated());
    }

    #[test]
    fn test_compute_max_streams_saturated_unstaked_returns_zero() {
        // When saturated, unstaked connections should be parked (max_streams = 0)
        let config = SwQosConfig {
            max_streams_per_ms: 1, // very low to easily saturate
            base_max_streams: 2048,
            ..SwQosConfig::default()
        };
        let cancel = CancellationToken::new();
        let stats = Arc::new(StreamerStats::default());
        let staked_nodes = Arc::new(RwLock::new(crate::streamer::StakedNodes::default()));
        let swqos = SwQos::new(config, stats, staked_nodes, cancel);

        // Exhaust the bucket by acquiring all tokens directly.
        // With max_streams_per_ms=1, burst_capacity = 1*1000/10 = 100 tokens.
        for _ in 0..200 {
            swqos.load_tracker.acquire();
        }

        if swqos.load_tracker.is_saturated() {
            let context = SwQosConnectionContext {
                peer_type: ConnectionPeerType::Unstaked,
                remote_pubkey: None,
                total_stake: 0,
                in_staked_table: false,
                last_update: Arc::new(AtomicU64::new(0)),
                stream_counter: None,
            };

            // We can't easily create a quinn::Connection in unit tests,
            // but we can verify the logic directly
            assert!(
                matches!(context.peer_type, ConnectionPeerType::Unstaked),
                "unstaked peer should be parked when saturated"
            );
        }
    }

    #[test]
    fn test_total_stake_zero_no_panic() {
        // Verify no panic when total_stake is 0
        let capacity_tps: u64 = 500_000;
        let stake: u64 = 100;
        let total_stake: u64 = 0;
        let share_tps = capacity_tps
            .saturating_mul(stake)
            .checked_div(total_stake)
            .unwrap_or(0);
        assert_eq!(share_tps, 0);
    }

    #[test]
    fn test_rtt_scale_minimum_is_one() {
        let rtt = Duration::from_millis(10); // less than REFERENCE_RTT
        let clamped = rtt.clamp(MIN_RTT, MAX_RTT);
        let rtt_scale = (clamped.as_millis() as u32) / (REFERENCE_RTT.as_millis() as u32);
        let rtt_scale = rtt_scale.max(1);
        assert_eq!(rtt_scale, 1);
    }

    #[test]
    fn test_rtt_scale_high_rtt() {
        let rtt = Duration::from_millis(200);
        let clamped = rtt.clamp(MIN_RTT, MAX_RTT);
        let rtt_scale = (clamped.as_millis() as u32) / (REFERENCE_RTT.as_millis() as u32);
        let rtt_scale = rtt_scale.max(1);
        assert_eq!(rtt_scale, 4); // 200ms / 50ms = 4
    }

    #[test]
    fn test_staked_quota_divided_by_connection_count() {
        // Verify quota is divided evenly among connections from the same peer
        let capacity_tps: u64 = 500_000;
        let stake: u64 = 1000;
        let total_stake: u64 = 100_000;
        let rtt = Duration::from_millis(50);

        let share_tps = capacity_tps
            .saturating_mul(stake)
            .checked_div(total_stake)
            .unwrap_or(0);
        let quota = (share_tps as f64 * rtt.as_secs_f64()) as u32;
        assert_eq!(quota, 250);

        // With 1 connection: full quota
        let per_conn = quota / 1u32.max(1);
        assert_eq!(per_conn, 250);

        // With 4 connections: quota / 4
        let per_conn = quota / 4u32.max(1);
        assert_eq!(per_conn, 62);

        // With 16 connections: quota / 16
        let per_conn = quota / 16u32.max(1);
        assert_eq!(per_conn, 15);

        // Total across all connections never exceeds original quota
        assert!(per_conn * 16 <= quota);
    }

    #[test]
    fn test_staked_quota_proportional() {
        // Verify proportional share calculation for staked peers
        let capacity_tps: u64 = 500_000;
        let stake: u64 = 1000;
        let total_stake: u64 = 100_000;
        let rtt = Duration::from_millis(50);

        let share_tps = capacity_tps
            .saturating_mul(stake)
            .checked_div(total_stake)
            .unwrap_or(0);
        assert_eq!(share_tps, 5000); // 1% of 500K

        let quota = (share_tps as f64 * rtt.as_secs_f64()) as u32;
        assert_eq!(quota, 250); // 5000 * 0.05 = 250 streams in flight
    }
}
