use {
    crate::{
        nonblocking::{qos::OpaqueStreamerCounter, quic::ConnectionPeerType},
        quic::StreamerStats,
    },
    percentage::Percentage,
    std::{
        sync::{
            Arc, RwLock,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
        time::{Duration, Instant},
    },
    tokio::time::sleep,
};

/// Max TPS allowed for unstaked connection
const MAX_UNSTAKED_TPS: u64 = 200;
/// Expected % of max TPS to be consumed by unstaked connections
const EXPECTED_UNSTAKED_STREAMS_PERCENT: u64 = 20;

pub const STREAM_THROTTLING_INTERVAL_MS: u64 = 100;
pub const STREAM_THROTTLING_INTERVAL: Duration =
    Duration::from_millis(STREAM_THROTTLING_INTERVAL_MS);
const STREAM_LOAD_EMA_INTERVAL_MS: u64 = 5;
// EMA smoothing window to reduce sensitivity to short-lived load spikes at the start
// of a leader slot. Throttling is only triggered when saturation is sustained.
// The value 40 was chosen based on simulations: at a max target TPS of ~400K,
// it allows the system to absorb a burst of ~50K transactions over ~40 ms
// before throttling activates.
const STREAM_LOAD_EMA_INTERVAL_COUNT: u64 = 40;

const STAKED_THROTTLING_ON_LOAD_THRESHOLD_PERCENT: u64 = 95;

pub(crate) struct StakedStreamLoadEMA {
    current_load_ema: AtomicU64,
    load_in_recent_interval: AtomicU64,
    last_update: RwLock<Instant>,
    stats: Arc<StreamerStats>,
    max_staked_load_in_throttling_window: u64,
    max_unstaked_load_in_throttling_window: u64,
    max_streams_per_ms: u64,
    staked_throttling_on_load_threshold: u64, // in streams/STREAM_LOAD_EMA_INTERVAL_MS
    staked_throttling_enabled: AtomicBool,
}

impl StakedStreamLoadEMA {
    pub(crate) fn new(
        stats: Arc<StreamerStats>,
        max_unstaked_connections: usize,
        max_streams_per_ms: u64,
    ) -> Self {
        let allow_unstaked_streams = max_unstaked_connections > 0;
        let max_staked_load_in_ms = if allow_unstaked_streams {
            max_streams_per_ms
                - Percentage::from(EXPECTED_UNSTAKED_STREAMS_PERCENT).apply_to(max_streams_per_ms)
        } else {
            max_streams_per_ms
        };

        let max_staked_load_in_ema_interval = max_staked_load_in_ms * STREAM_LOAD_EMA_INTERVAL_MS;
        let max_staked_load_in_throttling_window =
            max_staked_load_in_ms * STREAM_THROTTLING_INTERVAL_MS;

        let max_unstaked_load_in_throttling_window = if allow_unstaked_streams {
            MAX_UNSTAKED_TPS * STREAM_THROTTLING_INTERVAL_MS / 1000
        } else {
            0
        };

        let staked_throttling_on_load_threshold =
            Percentage::from(STAKED_THROTTLING_ON_LOAD_THRESHOLD_PERCENT)
                .apply_to(max_staked_load_in_ema_interval);

        Self {
            current_load_ema: AtomicU64::default(),
            load_in_recent_interval: AtomicU64::default(),
            last_update: RwLock::new(Instant::now()),
            stats,
            max_staked_load_in_throttling_window,
            max_unstaked_load_in_throttling_window,
            max_streams_per_ms,
            staked_throttling_on_load_threshold,
            staked_throttling_enabled: AtomicBool::new(false),
        }
    }

    fn ema_function(current_ema: u128, recent_load: u128) -> u128 {
        // Using the EMA multiplier helps in avoiding the floating point math during EMA related calculations
        const STREAM_LOAD_EMA_MULTIPLIER: u128 = 1024;
        let multiplied_smoothing_factor: u128 =
            2 * STREAM_LOAD_EMA_MULTIPLIER / (u128::from(STREAM_LOAD_EMA_INTERVAL_COUNT) + 1);

        // The formula is
        //    updated_ema = recent_load * smoothing_factor + current_ema * (1 - smoothing_factor)
        // To avoid floating point math, we are using STREAM_LOAD_EMA_MULTIPLIER
        //    updated_ema = (recent_load * multiplied_smoothing_factor
        //                   + current_ema * (multiplier - multiplied_smoothing_factor)) / multiplier
        (recent_load * multiplied_smoothing_factor
            + current_ema * (STREAM_LOAD_EMA_MULTIPLIER - multiplied_smoothing_factor))
            / STREAM_LOAD_EMA_MULTIPLIER
    }

    fn update_ema(&self, time_since_last_update_ms: u128) {
        // if time_since_last_update_ms > STREAM_LOAD_EMA_INTERVAL_MS, there might be intervals where ema was not updated.
        // count how many updates (1 + missed intervals) are needed.
        let num_extra_updates =
            time_since_last_update_ms.saturating_sub(1) / u128::from(STREAM_LOAD_EMA_INTERVAL_MS);

        let load_in_recent_interval =
            u128::from(self.load_in_recent_interval.swap(0, Ordering::Relaxed));

        let mut updated_load_ema = Self::ema_function(
            u128::from(self.current_load_ema.load(Ordering::Relaxed)),
            load_in_recent_interval,
        );

        for _ in 0..num_extra_updates {
            updated_load_ema = Self::ema_function(updated_load_ema, 0);
            if updated_load_ema == 0 {
                break;
            }
        }

        let Ok(updated_load_ema) = u64::try_from(updated_load_ema) else {
            error!("Failed to convert EMA {updated_load_ema} to a u64. Not updating the load EMA");
            self.stats
                .stream_load_ema_overflow
                .fetch_add(1, Ordering::Relaxed);
            return;
        };

        if self.staked_throttling_on_load_threshold > 0 {
            self.staked_throttling_enabled.store(
                updated_load_ema >= self.staked_throttling_on_load_threshold,
                Ordering::Relaxed,
            );
        }

        self.current_load_ema
            .store(updated_load_ema, Ordering::Relaxed);
        self.stats
            .stream_load_ema
            .store(updated_load_ema as usize, Ordering::Relaxed);
    }

    pub(crate) fn update_ema_if_needed(&self) {
        const EMA_DURATION: Duration = Duration::from_millis(STREAM_LOAD_EMA_INTERVAL_MS);
        // Read lock enables multiple connection handlers to run in parallel if interval is not expired
        if Instant::now().duration_since(*self.last_update.read().unwrap()) >= EMA_DURATION {
            let mut last_update_w = self.last_update.write().unwrap();
            // Recheck as some other thread might have updated the ema since this thread tried to acquire the write lock.
            let since_last_update = Instant::now().duration_since(*last_update_w);
            if since_last_update >= EMA_DURATION {
                *last_update_w = Instant::now();
                self.update_ema(since_last_update.as_millis());
            }
        }
    }

    pub(crate) fn increment_load(&self, peer_type: ConnectionPeerType) {
        if peer_type.is_staked() {
            self.load_in_recent_interval.fetch_add(1, Ordering::Relaxed);
        }
        self.update_ema_if_needed();
    }

    pub(crate) fn available_load_capacity_in_throttling_duration(
        &self,
        peer_type: ConnectionPeerType,
        total_stake: u64,
    ) -> u64 {
        match peer_type {
            ConnectionPeerType::Unstaked => self.max_unstaked_load_in_throttling_window,
            ConnectionPeerType::Staked(stake) => {
                if self.staked_throttling_enabled.load(Ordering::Relaxed) {
                    // 1 is added to `max_unstaked_load_in_throttling_window` to guarantee that staked
                    // clients get at least 1 more number of streams than unstaked connections.
                    self.max_staked_load_in_throttling_window
                        .saturating_mul(stake)
                        .checked_div(total_stake)
                        .unwrap_or(self.max_unstaked_load_in_throttling_window + 1)
                        .max(self.max_unstaked_load_in_throttling_window + 1)
                } else {
                    self.max_staked_load_in_throttling_window
                }
            }
        }
    }

    pub(crate) fn max_streams_per_ms(&self) -> u64 {
        self.max_streams_per_ms
    }
}

#[derive(Debug)]
pub struct ConnectionStreamCounter {
    pub(crate) stream_count: AtomicU64,
    last_throttling_instant: RwLock<tokio::time::Instant>,
}

impl OpaqueStreamerCounter for ConnectionStreamCounter {}

impl ConnectionStreamCounter {
    pub fn new() -> Self {
        Self {
            stream_count: AtomicU64::default(),
            last_throttling_instant: RwLock::new(tokio::time::Instant::now()),
        }
    }

    /// Reset the counter and last throttling instant and
    /// return last_throttling_instant regardless it is reset or not.
    pub(crate) fn reset_throttling_params_if_needed(&self) -> tokio::time::Instant {
        let last_throttling_instant = *self.last_throttling_instant.read().unwrap();
        if tokio::time::Instant::now().duration_since(last_throttling_instant)
            > STREAM_THROTTLING_INTERVAL
        {
            let mut last_throttling_instant = self.last_throttling_instant.write().unwrap();
            // Recheck as some other thread might have done throttling since this thread tried to acquire the write lock.
            if tokio::time::Instant::now().duration_since(*last_throttling_instant)
                > STREAM_THROTTLING_INTERVAL
            {
                *last_throttling_instant = tokio::time::Instant::now();
                self.stream_count.store(0, Ordering::Relaxed);
            }
            *last_throttling_instant
        } else {
            last_throttling_instant
        }
    }
}

pub(crate) async fn throttle_stream(
    stats: &StreamerStats,
    peer_type: ConnectionPeerType,
    remote_addr: std::net::SocketAddr,
    stream_counter: &Arc<ConnectionStreamCounter>,
    max_streams_per_throttling_interval: u64,
) {
    let throttle_interval_start = stream_counter.reset_throttling_params_if_needed();
    let streams_read_in_throttle_interval = stream_counter.stream_count.load(Ordering::Relaxed);
    if streams_read_in_throttle_interval >= max_streams_per_throttling_interval {
        // The peer is sending faster than we're willing to read. Sleep for what's
        // left of this read interval so the peer backs off.
        let throttle_duration =
            STREAM_THROTTLING_INTERVAL.saturating_sub(throttle_interval_start.elapsed());

        if !throttle_duration.is_zero() {
            debug!(
                "Throttling stream from {remote_addr:?}, peer type: {peer_type:?}, \
                 max_streams_per_interval: {max_streams_per_throttling_interval}, \
                 read_interval_streams: {streams_read_in_throttle_interval} throttle_duration: \
                 {throttle_duration:?}"
            );
            stats.throttled_streams.fetch_add(1, Ordering::Relaxed);
            match peer_type {
                ConnectionPeerType::Unstaked => {
                    stats
                        .throttled_unstaked_streams
                        .fetch_add(1, Ordering::Relaxed);
                }
                ConnectionPeerType::Staked(_) => {
                    stats
                        .throttled_staked_streams
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            sleep(throttle_duration).await;
        }
    }
}

#[cfg(test)]
pub mod test {
    use {
        super::*,
        crate::quic::{
            DEFAULT_MAX_STREAMS_PER_MS, DEFAULT_MAX_UNSTAKED_CONNECTIONS, StreamerStats,
        },
        std::sync::{Arc, atomic::Ordering},
    };

    #[test]
    fn test_max_streams_for_unstaked_connection() {
        let load_ema = Arc::new(StakedStreamLoadEMA::new(
            Arc::new(StreamerStats::default()),
            DEFAULT_MAX_UNSTAKED_CONNECTIONS,
            DEFAULT_MAX_STREAMS_PER_MS,
        ));
        assert_eq!(
            load_ema.available_load_capacity_in_throttling_duration(
                ConnectionPeerType::Unstaked,
                10000,
            ),
            20
        );
    }

    #[test]
    fn test_staked_throttling_on_off() {
        let mut load_ema = StakedStreamLoadEMA::new(
            Arc::new(StreamerStats::default()),
            DEFAULT_MAX_UNSTAKED_CONNECTIONS,
            DEFAULT_MAX_STREAMS_PER_MS,
        );

        load_ema.staked_throttling_on_load_threshold = 10;

        load_ema.current_load_ema.store(12, Ordering::Relaxed);
        load_ema
            .load_in_recent_interval
            .store(12, Ordering::Relaxed);
        load_ema.update_ema(u128::from(STREAM_LOAD_EMA_INTERVAL_MS));
        assert!(load_ema.staked_throttling_enabled.load(Ordering::Relaxed));

        load_ema.current_load_ema.store(4, Ordering::Relaxed);
        load_ema.load_in_recent_interval.store(0, Ordering::Relaxed);
        load_ema.update_ema(u128::from(STREAM_LOAD_EMA_INTERVAL_MS));
        assert!(!load_ema.staked_throttling_enabled.load(Ordering::Relaxed));
    }

    #[test]
    fn test_staked_capacity_shares_when_throttled() {
        let mut load_ema = StakedStreamLoadEMA::new(
            Arc::new(StreamerStats::default()),
            DEFAULT_MAX_UNSTAKED_CONNECTIONS,
            DEFAULT_MAX_STREAMS_PER_MS,
        );

        load_ema
            .staked_throttling_enabled
            .store(true, Ordering::Relaxed);
        load_ema.max_staked_load_in_throttling_window = 100;
        load_ema.max_unstaked_load_in_throttling_window = 20;

        assert_eq!(
            load_ema.available_load_capacity_in_throttling_duration(
                ConnectionPeerType::Staked(10),
                100
            ),
            load_ema.max_unstaked_load_in_throttling_window + 1
        );
        assert_eq!(
            load_ema.available_load_capacity_in_throttling_duration(
                ConnectionPeerType::Staked(50),
                100
            ),
            50
        );
    }

    #[test]
    fn test_no_throttle_below_threshold() {
        let mut load_ema = StakedStreamLoadEMA::new(
            Arc::new(StreamerStats::default()),
            DEFAULT_MAX_UNSTAKED_CONNECTIONS,
            DEFAULT_MAX_STREAMS_PER_MS,
        );

        load_ema
            .staked_throttling_enabled
            .store(false, Ordering::Relaxed);
        load_ema.max_staked_load_in_throttling_window = 100;
        load_ema.max_unstaked_load_in_throttling_window = 20;

        assert_eq!(
            load_ema.available_load_capacity_in_throttling_duration(
                ConnectionPeerType::Staked(10),
                100
            ),
            load_ema.max_staked_load_in_throttling_window
        );
    }

    #[test]
    fn test_ema_decay_handles_missing_intervals() {
        let load_ema = StakedStreamLoadEMA::new(
            Arc::new(StreamerStats::default()),
            DEFAULT_MAX_UNSTAKED_CONNECTIONS,
            DEFAULT_MAX_STREAMS_PER_MS,
        );

        load_ema.current_load_ema.store(100, Ordering::Relaxed);
        load_ema
            .load_in_recent_interval
            .store(100, Ordering::Relaxed);

        load_ema.update_ema(u128::from(STREAM_LOAD_EMA_INTERVAL_MS * 3));

        let expected = StakedStreamLoadEMA::ema_function(
            StakedStreamLoadEMA::ema_function(StakedStreamLoadEMA::ema_function(100, 100), 0),
            0,
        );
        assert_eq!(
            load_ema.current_load_ema.load(Ordering::Relaxed),
            u64::try_from(expected).unwrap()
        );
    }

    #[test]
    fn test_total_stake_zero_safety() {
        let load_ema = StakedStreamLoadEMA::new(
            Arc::new(StreamerStats::default()),
            DEFAULT_MAX_UNSTAKED_CONNECTIONS,
            DEFAULT_MAX_STREAMS_PER_MS,
        );
        load_ema
            .staked_throttling_enabled
            .store(true, Ordering::Relaxed);

        assert_eq!(
            load_ema
                .available_load_capacity_in_throttling_duration(ConnectionPeerType::Staked(10), 0),
            load_ema.max_unstaked_load_in_throttling_window + 1
        );
    }
}
