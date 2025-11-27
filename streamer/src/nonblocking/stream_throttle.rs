use {
    crate::{
        nonblocking::{qos::OpaqueStreamerCounter, quic::ConnectionPeerType},
        quic::StreamerStats,
    },
    percentage::Percentage,
    std::{
        cmp,
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc, RwLock,
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
const STREAM_LOAD_EMA_INTERVAL_COUNT: u64 = 10;
const EMA_WINDOW_MS: u64 = STREAM_LOAD_EMA_INTERVAL_MS * STREAM_LOAD_EMA_INTERVAL_COUNT;

pub(crate) struct StakedStreamLoadEMA {
    current_load_ema: AtomicU64,
    load_in_recent_interval: AtomicU64,
    last_update: RwLock<Instant>,
    stats: Arc<StreamerStats>,
    // Maximum number of streams for a staked connection in EMA window
    // Note: EMA window can be different than stream throttling window. EMA is being calculated
    //       specifically for staked connections. Unstaked connections have fixed limit on
    //       stream load, which is tracked by `max_unstaked_load_in_throttling_window` field.
    max_staked_load_in_ema_window: u64,
    // Maximum number of streams for an unstaked connection in stream throttling window
    max_unstaked_load_in_throttling_window: u64,
    max_streams_per_ms: u64,
}

impl StakedStreamLoadEMA {
    pub(crate) fn new(
        stats: Arc<StreamerStats>,
        max_unstaked_connections: usize,
        max_streams_per_ms: u64,
    ) -> Self {
        let allow_unstaked_streams = max_unstaked_connections > 0;
        let max_staked_load_in_ema_window = if allow_unstaked_streams {
            (max_streams_per_ms
                - Percentage::from(EXPECTED_UNSTAKED_STREAMS_PERCENT).apply_to(max_streams_per_ms))
                * EMA_WINDOW_MS
        } else {
            max_streams_per_ms * EMA_WINDOW_MS
        };

        let max_unstaked_load_in_throttling_window = if allow_unstaked_streams {
            MAX_UNSTAKED_TPS * STREAM_THROTTLING_INTERVAL_MS / 1000
        } else {
            0
        };

        Self {
            current_load_ema: AtomicU64::default(),
            load_in_recent_interval: AtomicU64::default(),
            last_update: RwLock::new(Instant::now()),
            stats,
            max_staked_load_in_ema_window,
            max_unstaked_load_in_throttling_window,
            max_streams_per_ms,
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
            updated_load_ema = Self::ema_function(updated_load_ema, load_in_recent_interval);
        }

        let Ok(updated_load_ema) = u64::try_from(updated_load_ema) else {
            error!("Failed to convert EMA {updated_load_ema} to a u64. Not updating the load EMA");
            self.stats
                .stream_load_ema_overflow
                .fetch_add(1, Ordering::Relaxed);
            return;
        };

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
                // If the current load is low, cap it to 25% of max_load.
                let current_load = u128::from(cmp::max(
                    self.current_load_ema.load(Ordering::Relaxed),
                    self.max_staked_load_in_ema_window / 4,
                ));

                // Formula is (max_load ^ 2 / current_load) * (stake / total_stake)
                let capacity_in_ema_window = (u128::from(self.max_staked_load_in_ema_window)
                    * u128::from(self.max_staked_load_in_ema_window)
                    * u128::from(stake))
                    / (current_load * u128::from(total_stake));

                let calculated_capacity = capacity_in_ema_window
                    * u128::from(STREAM_THROTTLING_INTERVAL_MS)
                    / u128::from(EMA_WINDOW_MS);
                let calculated_capacity = u64::try_from(calculated_capacity).unwrap_or_else(|_| {
                    error!(
                        "Failed to convert stream capacity {calculated_capacity} to u64. Using \
                         minimum load capacity"
                    );
                    self.stats
                        .stream_load_capacity_overflow
                        .fetch_add(1, Ordering::Relaxed);
                    self.max_unstaked_load_in_throttling_window
                        .saturating_add(1)
                });

                // 1 is added to `max_unstaked_load_in_throttling_window` to guarantee that staked
                // clients get at least 1 more number of streams than unstaked connections.
                cmp::max(
                    calculated_capacity,
                    self.max_unstaked_load_in_throttling_window
                        .saturating_add(1),
                )
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
        crate::{
            nonblocking::stream_throttle::STREAM_LOAD_EMA_INTERVAL_MS,
            quic::{StreamerStats, DEFAULT_MAX_STREAMS_PER_MS, DEFAULT_MAX_UNSTAKED_CONNECTIONS},
        },
        std::{
            sync::{atomic::Ordering, Arc},
            time::{Duration, Instant},
        },
    };

    #[test]
    fn test_max_streams_for_unstaked_connection() {
        let load_ema = Arc::new(StakedStreamLoadEMA::new(
            Arc::new(StreamerStats::default()),
            DEFAULT_MAX_UNSTAKED_CONNECTIONS,
            DEFAULT_MAX_STREAMS_PER_MS,
        ));
        // 50K packets per ms * 20% / 500 max unstaked connections
        assert_eq!(
            load_ema.available_load_capacity_in_throttling_duration(
                ConnectionPeerType::Unstaked,
                10000,
            ),
            20
        );
    }

    #[test]
    fn test_max_streams_for_staked_connection() {
        let load_ema = Arc::new(StakedStreamLoadEMA::new(
            Arc::new(StreamerStats::default()),
            DEFAULT_MAX_UNSTAKED_CONNECTIONS,
            DEFAULT_MAX_STREAMS_PER_MS,
        ));

        // EMA load is used for staked connections to calculate max number of allowed streams.
        // EMA window = 5ms interval * 10 intervals = 50ms
        // max streams per window = 500K streams/sec * 80% = 400K/sec = 20K per 50ms
        // max_streams in 50ms = ((20K * 20K) / ema_load) * stake / total_stake
        //
        // Stream throttling window is 100ms. So it'll double the amount of max streams.
        // max_streams in 100ms (throttling window) = 2 * ((20K * 20K) / ema_load) * stake / total_stake

        load_ema.current_load_ema.store(20000, Ordering::Relaxed);
        // ema_load = 20K, stake = 15, total_stake = 10K
        // max_streams in 100ms (throttling window) = 2 * ((20K * 20K) / 20K) * 15 / 10K  = 60
        assert_eq!(
            load_ema.available_load_capacity_in_throttling_duration(
                ConnectionPeerType::Staked(15),
                10000,
            ),
            60
        );

        // ema_load = 20K, stake = 1K, total_stake = 10K
        // max_streams in 100ms (throttling window) = 2 * ((20K * 20K) / 20K) * 1K / 10K  = 4K
        assert_eq!(
            load_ema.available_load_capacity_in_throttling_duration(
                ConnectionPeerType::Staked(1000),
                10000,
            ),
            4000
        );

        load_ema.current_load_ema.store(5000, Ordering::Relaxed);
        // ema_load = 5K, stake = 15, total_stake = 10K
        // max_streams in 100ms (throttling window) = 2 * ((20K * 20K) / 5K) * 15 / 10K  = 240
        assert_eq!(
            load_ema.available_load_capacity_in_throttling_duration(
                ConnectionPeerType::Staked(15),
                10000,
            ),
            240
        );

        // ema_load = 5K, stake = 1K, total_stake = 10K
        // max_streams in 100ms (throttling window) = 2 * ((20K * 20K) / 5K) * 1K / 10K  = 16000
        assert_eq!(
            load_ema.available_load_capacity_in_throttling_duration(
                ConnectionPeerType::Staked(1000),
                10000,
            ),
            16000
        );

        // At 4000, the load is less than 25% of max_load (20K).
        // Test that we cap it to 25%, yielding the same result as if load was 5000.
        load_ema.current_load_ema.store(4000, Ordering::Relaxed);
        // function = ((20K * 20K) / 25% of 20K) * stake / total_stake
        assert_eq!(
            load_ema.available_load_capacity_in_throttling_duration(
                ConnectionPeerType::Staked(15),
                10000,
            ),
            240
        );

        // function = ((20K * 20K) / 25% of 20K) * stake / total_stake
        assert_eq!(
            load_ema.available_load_capacity_in_throttling_duration(
                ConnectionPeerType::Staked(1000),
                10000,
            ),
            16000
        );

        // At 1/40000 stake weight, and minimum load, it should still allow
        // max_unstaked_load_in_throttling_window + 1 streams.
        assert_eq!(
            load_ema.available_load_capacity_in_throttling_duration(
                ConnectionPeerType::Staked(1),
                40000,
            ),
            load_ema
                .max_unstaked_load_in_throttling_window
                .saturating_add(1)
        );
    }

    #[test]
    fn test_max_streams_for_staked_connection_with_no_unstaked_connections() {
        let load_ema = Arc::new(StakedStreamLoadEMA::new(
            Arc::new(StreamerStats::default()),
            0,
            DEFAULT_MAX_STREAMS_PER_MS,
        ));

        // EMA load is used for staked connections to calculate max number of allowed streams.
        // EMA window = 5ms interval * 10 intervals = 50ms
        // max streams per window = 500K streams/sec = 25K per 50ms
        // max_streams in 50ms = ((25K * 25K) / ema_load) * stake / total_stake
        //
        // Stream throttling window is 100ms. So it'll double the amount of max streams.
        // max_streams in 100ms (throttling window) = 2 * ((25K * 25K) / ema_load) * stake / total_stake

        load_ema.current_load_ema.store(20000, Ordering::Relaxed);
        // ema_load = 20K, stake = 15, total_stake = 10K
        // max_streams in 100ms (throttling window) = 2 * ((25K * 25K) / 20K) * 15 / 10K  = 93.75
        // Loss of precision occurs here because max streams is computed for 50ms window and then doubled.
        assert!(
            (92u64..=94).contains(&load_ema.available_load_capacity_in_throttling_duration(
                ConnectionPeerType::Staked(15),
                10000
            ))
        );

        // ema_load = 20K, stake = 1K, total_stake = 10K
        // max_streams in 100ms (throttling window) = 2 * ((25K * 25K) / 20K) * 1K / 10K  = 6250
        assert!((6249u64..=6250).contains(
            &load_ema.available_load_capacity_in_throttling_duration(
                ConnectionPeerType::Staked(1000),
                10000
            )
        ));

        load_ema.current_load_ema.store(10000, Ordering::Relaxed);
        // ema_load = 10K, stake = 15, total_stake = 10K
        // max_streams in 100ms (throttling window) = 2 * ((25K * 25K) / 10K) * 15 / 10K  = 187.5
        // Loss of precision occurs here because max streams is computed for 50ms window and then doubled.
        assert!(
            (186u64..=188).contains(&load_ema.available_load_capacity_in_throttling_duration(
                ConnectionPeerType::Staked(15),
                10000
            ))
        );

        // ema_load = 10K, stake = 1K, total_stake = 10K
        // max_streams in 100ms (throttling window) = 2 * ((25K * 25K) / 10K) * 1K / 10K  = 12500
        assert!((12499u64..=12500).contains(
            &load_ema.available_load_capacity_in_throttling_duration(
                ConnectionPeerType::Staked(1000),
                10000
            )
        ));

        // At 4000, the load is less than 25% of max_load (25K).
        // Test that we cap it to 25%, yielding the same result as if load was 25K/4.
        load_ema.current_load_ema.store(4000, Ordering::Relaxed);
        // function = ((20K * 20K) / 25% of 25K) * stake / total_stake
        assert_eq!(
            load_ema.available_load_capacity_in_throttling_duration(
                ConnectionPeerType::Staked(15),
                10000
            ),
            300
        );

        // function = ((25K * 25K) / 25% of 25K) * stake / total_stake
        assert_eq!(
            load_ema.available_load_capacity_in_throttling_duration(
                ConnectionPeerType::Staked(1000),
                10000
            ),
            20000
        );

        // At 1/400000 stake weight, and minimum load, it should still allow
        // max_unstaked_load_in_throttling_window + 1 streams.
        assert_eq!(
            load_ema.available_load_capacity_in_throttling_duration(
                ConnectionPeerType::Staked(1),
                400000
            ),
            load_ema
                .max_unstaked_load_in_throttling_window
                .saturating_add(1)
        );
    }

    #[test]
    fn test_update_ema() {
        let stream_load_ema = Arc::new(StakedStreamLoadEMA::new(
            Arc::new(StreamerStats::default()),
            DEFAULT_MAX_UNSTAKED_CONNECTIONS,
            DEFAULT_MAX_STREAMS_PER_MS,
        ));
        stream_load_ema
            .load_in_recent_interval
            .store(2500, Ordering::Relaxed);
        stream_load_ema
            .current_load_ema
            .store(2000, Ordering::Relaxed);

        stream_load_ema.update_ema(5);

        let updated_ema = stream_load_ema.current_load_ema.load(Ordering::Relaxed);
        assert_eq!(updated_ema, 2090);

        stream_load_ema
            .load_in_recent_interval
            .store(2500, Ordering::Relaxed);

        stream_load_ema.update_ema(5);

        let updated_ema = stream_load_ema.current_load_ema.load(Ordering::Relaxed);
        assert_eq!(updated_ema, 2164);
    }

    #[test]
    fn test_update_ema_missing_interval() {
        let stream_load_ema = Arc::new(StakedStreamLoadEMA::new(
            Arc::new(StreamerStats::default()),
            DEFAULT_MAX_UNSTAKED_CONNECTIONS,
            DEFAULT_MAX_STREAMS_PER_MS,
        ));
        stream_load_ema
            .load_in_recent_interval
            .store(2500, Ordering::Relaxed);
        stream_load_ema
            .current_load_ema
            .store(2000, Ordering::Relaxed);

        stream_load_ema.update_ema(8);

        let updated_ema = stream_load_ema.current_load_ema.load(Ordering::Relaxed);
        assert_eq!(updated_ema, 2164);
    }

    #[test]
    fn test_update_ema_if_needed() {
        let stream_load_ema = Arc::new(StakedStreamLoadEMA::new(
            Arc::new(StreamerStats::default()),
            DEFAULT_MAX_UNSTAKED_CONNECTIONS,
            DEFAULT_MAX_STREAMS_PER_MS,
        ));
        stream_load_ema
            .load_in_recent_interval
            .store(2500, Ordering::Relaxed);
        stream_load_ema
            .current_load_ema
            .store(2000, Ordering::Relaxed);

        stream_load_ema.update_ema_if_needed();

        let updated_ema = stream_load_ema.current_load_ema.load(Ordering::Relaxed);
        assert_eq!(updated_ema, 2000);

        let ema_interval = Duration::from_millis(STREAM_LOAD_EMA_INTERVAL_MS);
        *stream_load_ema.last_update.write().unwrap() =
            Instant::now().checked_sub(ema_interval).unwrap();

        stream_load_ema.update_ema_if_needed();
        assert!(
            Instant::now().duration_since(*stream_load_ema.last_update.read().unwrap())
                < ema_interval
        );

        let updated_ema = stream_load_ema.current_load_ema.load(Ordering::Relaxed);
        assert_eq!(updated_ema, 2090);
    }
}
