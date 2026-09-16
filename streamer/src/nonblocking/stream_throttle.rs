use {
    crate::{
        nonblocking::{qos::OpaqueStreamerCounter, quic::ConnectionPeerType},
        quic::StreamerStats,
    },
    std::{
        sync::{
            Arc, RwLock,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
        time::{Duration, Instant},
    },
    tokio::time::sleep,
};

/// Max TPS per unstaked peer while total load is below
/// `UNSTAKED_THROTTLING_ON_LOAD_THRESHOLD_RATIO` of capacity.
pub(crate) const MAX_UNSTAKED_TPS: u64 = 500;
/// Max TPS per unstaked peer once total load is above that threshold. This is
/// the previous fixed quota, so behavior under saturation is unchanged.
const MIN_UNSTAKED_TPS: u64 = 200;

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

/// Fraction of capacity at which staked peers switch to stake-proportional
/// quotas. Compared against staked load alone, so unstaked traffic never
/// throttles staked peers.
///
/// 0.76 is the previous trip point of 95% of an 80% staked share (1900
/// streams per 5 ms interval at the default 500 streams/ms), kept for now so
/// staked throttling starts at the same load while unstaked load accounting
/// is rolled out.
const STAKED_THROTTLING_ON_LOAD_THRESHOLD_RATIO: f64 = 0.76;
/// With unstaked connections disabled there was no staked share to shrink,
/// so the previous 0.95 still applies.
const STAKED_THROTTLING_ON_LOAD_WITHOUT_UNSTAKED_THRESHOLD_RATIO: f64 = 0.95;
/// Fraction of capacity at which unstaked peers fall back to their minimum
/// quota. Compared against total (staked + unstaked) load, so unstaked peers
/// yield to staked demand and may use the whole pool otherwise. Kept below
/// the staked ratio so unstaked peers back off first.
const UNSTAKED_THROTTLING_ON_LOAD_THRESHOLD_RATIO: f64 = 0.70;
const _: () = assert!(
    UNSTAKED_THROTTLING_ON_LOAD_THRESHOLD_RATIO <= STAKED_THROTTLING_ON_LOAD_THRESHOLD_RATIO
);

/// Tracks stream load as exponential moving averages, kept separately for
/// staked and unstaked streams. Both EMAs are advanced in the same update so
/// they share a time grid and their sum is the total load.
///
/// Stream capacity is a single pool in which staked peers have priority:
/// staked throttling is driven by staked load alone, unstaked throttling by
/// total load. Quotas are per peer (see `ConnectionStreamCounter`).
pub(crate) struct StreamLoadEMA {
    staked_load_ema: AtomicU64,
    staked_load_in_recent_interval: AtomicU64,
    unstaked_load_ema: AtomicU64,
    unstaked_load_in_recent_interval: AtomicU64,
    last_update: RwLock<Instant>,
    stats: Arc<StreamerStats>,
    /// Capacity per throttling window. Also the staked quota while staked
    /// throttling is off, and the base for stake-proportional quotas while on.
    max_load_in_throttling_window: u64,
    /// Unstaked quota while unstaked throttling is off.
    max_unstaked_load_in_throttling_window: u64,
    /// Unstaked quota while unstaked throttling is on.
    min_unstaked_load_in_throttling_window: u64,
    max_streams_per_ms: u64,
    staked_throttling_on_load_threshold: u64, // in streams/STREAM_LOAD_EMA_INTERVAL_MS
    unstaked_throttling_on_load_threshold: u64, // in streams/STREAM_LOAD_EMA_INTERVAL_MS
    staked_throttling_enabled: AtomicBool,
    unstaked_throttling_enabled: AtomicBool,
}

impl StreamLoadEMA {
    pub(crate) fn new(
        stats: Arc<StreamerStats>,
        max_unstaked_connections: usize,
        max_streams_per_ms: u64,
    ) -> Self {
        let max_load_in_ema_interval = max_streams_per_ms * STREAM_LOAD_EMA_INTERVAL_MS;
        let max_load_in_throttling_window = max_streams_per_ms * STREAM_THROTTLING_INTERVAL_MS;

        let allow_unstaked_streams = max_unstaked_connections > 0;
        let (max_unstaked_load_in_throttling_window, min_unstaked_load_in_throttling_window) =
            if allow_unstaked_streams {
                (
                    MAX_UNSTAKED_TPS * STREAM_THROTTLING_INTERVAL_MS / 1000,
                    MIN_UNSTAKED_TPS * STREAM_THROTTLING_INTERVAL_MS / 1000,
                )
            } else {
                (0, 0)
            };

        let staked_threshold_ratio = if allow_unstaked_streams {
            STAKED_THROTTLING_ON_LOAD_THRESHOLD_RATIO
        } else {
            STAKED_THROTTLING_ON_LOAD_WITHOUT_UNSTAKED_THRESHOLD_RATIO
        };
        let staked_throttling_on_load_threshold =
            (staked_threshold_ratio * max_load_in_ema_interval as f64) as u64;
        let unstaked_throttling_on_load_threshold = (UNSTAKED_THROTTLING_ON_LOAD_THRESHOLD_RATIO
            * (max_load_in_ema_interval as f64))
            as u64;

        Self {
            staked_load_ema: AtomicU64::default(),
            staked_load_in_recent_interval: AtomicU64::default(),
            unstaked_load_ema: AtomicU64::default(),
            unstaked_load_in_recent_interval: AtomicU64::default(),
            last_update: RwLock::new(Instant::now()),
            stats,
            max_load_in_throttling_window,
            max_unstaked_load_in_throttling_window,
            min_unstaked_load_in_throttling_window,
            max_streams_per_ms,
            staked_throttling_on_load_threshold,
            unstaked_throttling_on_load_threshold,
            staked_throttling_enabled: AtomicBool::new(false),
            unstaked_throttling_enabled: AtomicBool::new(false),
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

        // Reset both counters before advancing either EMA so the two
        // estimates cover the same interval.
        let staked_load_in_recent_interval = self
            .staked_load_in_recent_interval
            .swap(0, Ordering::Relaxed);
        let unstaked_load_in_recent_interval = self
            .unstaked_load_in_recent_interval
            .swap(0, Ordering::Relaxed);

        if let Some(staked_load_ema) = self.advance_ema(
            &self.staked_load_ema,
            staked_load_in_recent_interval,
            num_extra_updates,
        ) {
            if self.staked_throttling_on_load_threshold > 0 {
                self.staked_throttling_enabled.store(
                    staked_load_ema >= self.staked_throttling_on_load_threshold,
                    Ordering::Relaxed,
                );
            }
            self.stats
                .staked_stream_load_ema
                .store(staked_load_ema as usize, Ordering::Relaxed);
        }

        if let Some(unstaked_load_ema) = self.advance_ema(
            &self.unstaked_load_ema,
            unstaked_load_in_recent_interval,
            num_extra_updates,
        ) {
            if self.unstaked_throttling_on_load_threshold > 0 {
                // Throttle unstaked peers on total load so they yield to
                // staked demand.
                let total_load_ema = self
                    .staked_load_ema
                    .load(Ordering::Relaxed)
                    .saturating_add(unstaked_load_ema);
                self.unstaked_throttling_enabled.store(
                    total_load_ema >= self.unstaked_throttling_on_load_threshold,
                    Ordering::Relaxed,
                );
            }
            self.stats
                .unstaked_stream_load_ema
                .store(unstaked_load_ema as usize, Ordering::Relaxed);
        }
    }

    /// Advances `ema` by one interval carrying `recent_load`, followed by
    /// `num_extra_updates` empty intervals. Returns the new value, or `None`
    /// if it does not fit in a u64, in which case `ema` is left unchanged.
    fn advance_ema(
        &self,
        ema: &AtomicU64,
        recent_load: u64,
        num_extra_updates: u128,
    ) -> Option<u64> {
        let mut updated_ema = Self::ema_function(
            u128::from(ema.load(Ordering::Relaxed)),
            u128::from(recent_load),
        );

        for _ in 0..num_extra_updates {
            updated_ema = Self::ema_function(updated_ema, 0);
            if updated_ema == 0 {
                break;
            }
        }

        match u64::try_from(updated_ema) {
            Ok(updated_ema) => {
                ema.store(updated_ema, Ordering::Relaxed);
                Some(updated_ema)
            }
            Err(_) => {
                error!("Failed to convert EMA {updated_ema} to a u64. Not updating the load EMA");
                self.stats
                    .stream_load_ema_overflow
                    .fetch_add(1, Ordering::Relaxed);
                None
            }
        }
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
        let load_in_recent_interval = if peer_type.is_staked() {
            &self.staked_load_in_recent_interval
        } else {
            &self.unstaked_load_in_recent_interval
        };
        load_in_recent_interval.fetch_add(1, Ordering::Relaxed);
        self.update_ema_if_needed();
    }

    /// Streams a peer of `peer_type` may open per throttling window.
    pub(crate) fn available_load_capacity_in_throttling_duration(
        &self,
        peer_type: ConnectionPeerType,
        total_stake: u64,
    ) -> u64 {
        match peer_type {
            ConnectionPeerType::Unstaked => {
                if self.unstaked_throttling_enabled.load(Ordering::Relaxed) {
                    self.min_unstaked_load_in_throttling_window
                } else {
                    self.max_unstaked_load_in_throttling_window
                }
            }
            ConnectionPeerType::Staked(stake) => {
                if self.staked_throttling_enabled.load(Ordering::Relaxed) {
                    // Staked throttling implies unstaked throttling, so unstaked peers are being
                    // throttled here. +1 guarantees staked always get a bit more.
                    let min_staked_load = self.min_unstaked_load_in_throttling_window + 1;
                    u128::from(self.max_load_in_throttling_window)
                        .saturating_mul(u128::from(stake))
                        .checked_div(u128::from(total_stake))
                        .and_then(|capacity| u64::try_from(capacity).ok())
                        .unwrap_or(min_staked_load)
                        .max(min_staked_load)
                } else {
                    self.max_load_in_throttling_window
                }
            }
        }
    }

    pub(crate) fn max_streams_per_ms(&self) -> u64 {
        self.max_streams_per_ms
    }
}

/// Per-peer stream counter for throttling. Shared by all connections under the
/// same connection-table key (the peer's pubkey when known, otherwise its IP
/// address), so quotas apply per peer, not per connection.
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

    const LAMPORTS_PER_SOL: u64 = 1_000_000_000;
    const TEST_TOTAL_STAKE: u64 = 400_000_000 * LAMPORTS_PER_SOL;
    // Matches the production default.
    const TEST_MAX_STREAMS_PER_MS: u64 = 500;

    fn new_throttled_load_ema(allow_unstaked_connections: bool) -> StreamLoadEMA {
        let load_ema = StreamLoadEMA::new(
            Arc::new(StreamerStats::default()),
            usize::from(allow_unstaked_connections),
            TEST_MAX_STREAMS_PER_MS,
        );
        load_ema
            .staked_throttling_enabled
            .store(true, Ordering::Relaxed);
        load_ema
    }

    #[test]
    fn test_max_streams_for_unstaked_connection() {
        let load_ema = Arc::new(StreamLoadEMA::new(
            Arc::new(StreamerStats::default()),
            DEFAULT_MAX_UNSTAKED_CONNECTIONS,
            DEFAULT_MAX_STREAMS_PER_MS,
        ));
        assert_eq!(
            load_ema.available_load_capacity_in_throttling_duration(
                ConnectionPeerType::Unstaked,
                10000,
            ),
            50
        );

        load_ema
            .unstaked_throttling_enabled
            .store(true, Ordering::Relaxed);
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
        let mut load_ema = StreamLoadEMA::new(
            Arc::new(StreamerStats::default()),
            DEFAULT_MAX_UNSTAKED_CONNECTIONS,
            DEFAULT_MAX_STREAMS_PER_MS,
        );

        load_ema.staked_throttling_on_load_threshold = 10;

        load_ema.staked_load_ema.store(12, Ordering::Relaxed);
        load_ema
            .staked_load_in_recent_interval
            .store(12, Ordering::Relaxed);
        load_ema.update_ema(u128::from(STREAM_LOAD_EMA_INTERVAL_MS));
        assert!(load_ema.staked_throttling_enabled.load(Ordering::Relaxed));

        load_ema.staked_load_ema.store(4, Ordering::Relaxed);
        load_ema
            .staked_load_in_recent_interval
            .store(0, Ordering::Relaxed);
        load_ema.update_ema(u128::from(STREAM_LOAD_EMA_INTERVAL_MS));
        assert!(!load_ema.staked_throttling_enabled.load(Ordering::Relaxed));
    }

    #[test]
    fn test_staked_capacity_shares_with_large_stakes() {
        let load_ema = new_throttled_load_ema(true);
        // Stake divisors below assume these window values.
        let full_staked_capacity = load_ema.max_load_in_throttling_window;
        assert_eq!(full_staked_capacity, 50_000);
        assert_eq!(load_ema.max_unstaked_load_in_throttling_window, 50);
        assert_eq!(load_ema.min_unstaked_load_in_throttling_window, 20);

        assert_eq!(
            load_ema.available_load_capacity_in_throttling_duration(
                ConnectionPeerType::Staked(1),
                TEST_TOTAL_STAKE,
            ),
            load_ema.min_unstaked_load_in_throttling_window + 1,
            "any staked client gets more than throttled unstaked",
        );

        for stake_divisor in [
            // 1_100 and 1_000 represent below and above the u64 multiplication-overflow boundary.
            1_500, 1_100, 1_000, 400, 50, 20, 1,
        ] {
            assert_eq!(
                load_ema.available_load_capacity_in_throttling_duration(
                    ConnectionPeerType::Staked(TEST_TOTAL_STAKE / stake_divisor),
                    TEST_TOTAL_STAKE,
                ),
                full_staked_capacity / stake_divisor,
                "incorrect capacity for {} SOL of stake",
                TEST_TOTAL_STAKE / stake_divisor / LAMPORTS_PER_SOL,
            );
        }
    }

    #[test]
    fn test_staked_capacity_shares_with_large_stakes_and_no_unstaked_connections() {
        let load_ema = new_throttled_load_ema(false);
        // Stake divisors below assume these window values.
        let full_staked_capacity = load_ema.max_load_in_throttling_window;
        assert_eq!(full_staked_capacity, 50_000);
        assert_eq!(load_ema.max_unstaked_load_in_throttling_window, 0);
        assert_eq!(load_ema.min_unstaked_load_in_throttling_window, 0);

        assert_eq!(
            load_ema.available_load_capacity_in_throttling_duration(
                ConnectionPeerType::Staked(100),
                TEST_TOTAL_STAKE,
            ),
            load_ema.min_unstaked_load_in_throttling_window + 1,
            "any staked client gets more than unstaked",
        );

        for stake_divisor in [
            // 1_100 and 1_000 represent below and above the u64 multiplication-overflow boundary.
            1_500, 1_100, 1_000, 400, 50, 20, 1,
        ] {
            assert_eq!(
                load_ema.available_load_capacity_in_throttling_duration(
                    ConnectionPeerType::Staked(TEST_TOTAL_STAKE / stake_divisor),
                    TEST_TOTAL_STAKE,
                ),
                full_staked_capacity / stake_divisor,
                "incorrect capacity for {} SOL of stake",
                TEST_TOTAL_STAKE / stake_divisor / LAMPORTS_PER_SOL,
            );
        }
    }

    #[test]
    fn test_staked_capacity_with_maximum_stake_values() {
        let load_ema = new_throttled_load_ema(true);

        assert_eq!(
            load_ema.available_load_capacity_in_throttling_duration(
                ConnectionPeerType::Staked(u64::MAX),
                u64::MAX,
            ),
            load_ema.max_load_in_throttling_window,
        );
    }

    #[test]
    fn test_no_throttle_below_threshold() {
        let mut load_ema = StreamLoadEMA::new(
            Arc::new(StreamerStats::default()),
            DEFAULT_MAX_UNSTAKED_CONNECTIONS,
            DEFAULT_MAX_STREAMS_PER_MS,
        );

        load_ema
            .staked_throttling_enabled
            .store(false, Ordering::Relaxed);
        load_ema.max_load_in_throttling_window = 100;
        load_ema.max_unstaked_load_in_throttling_window = 20;

        assert_eq!(
            load_ema.available_load_capacity_in_throttling_duration(
                ConnectionPeerType::Staked(10),
                100
            ),
            load_ema.max_load_in_throttling_window
        );
    }

    #[test]
    fn test_ema_decay_handles_missing_intervals() {
        let load_ema = StreamLoadEMA::new(
            Arc::new(StreamerStats::default()),
            DEFAULT_MAX_UNSTAKED_CONNECTIONS,
            DEFAULT_MAX_STREAMS_PER_MS,
        );

        load_ema.staked_load_ema.store(100, Ordering::Relaxed);
        load_ema
            .staked_load_in_recent_interval
            .store(100, Ordering::Relaxed);

        load_ema.update_ema(u128::from(STREAM_LOAD_EMA_INTERVAL_MS * 3));

        let expected = StreamLoadEMA::ema_function(
            StreamLoadEMA::ema_function(StreamLoadEMA::ema_function(100, 100), 0),
            0,
        );
        assert_eq!(
            load_ema.staked_load_ema.load(Ordering::Relaxed),
            u64::try_from(expected).unwrap()
        );
    }

    #[test]
    fn test_unstaked_load_tracked_separately() {
        let mut load_ema = StreamLoadEMA::new(
            Arc::new(StreamerStats::default()),
            DEFAULT_MAX_UNSTAKED_CONNECTIONS,
            DEFAULT_MAX_STREAMS_PER_MS,
        );
        load_ema.staked_throttling_on_load_threshold = 40;
        load_ema.unstaked_throttling_on_load_threshold = 10;

        load_ema
            .staked_load_in_recent_interval
            .store(100, Ordering::Relaxed);
        load_ema
            .unstaked_load_in_recent_interval
            .store(1000, Ordering::Relaxed);
        load_ema.update_ema(u128::from(STREAM_LOAD_EMA_INTERVAL_MS));

        // Both counters are consumed by the same update.
        assert_eq!(
            load_ema
                .staked_load_in_recent_interval
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(
            load_ema
                .unstaked_load_in_recent_interval
                .load(Ordering::Relaxed),
            0
        );

        let expected_staked = u64::try_from(StreamLoadEMA::ema_function(0, 100)).unwrap();
        let expected_unstaked = u64::try_from(StreamLoadEMA::ema_function(0, 1000)).unwrap();
        assert_eq!(
            load_ema.staked_load_ema.load(Ordering::Relaxed),
            expected_staked
        );
        assert_eq!(
            load_ema.unstaked_load_ema.load(Ordering::Relaxed),
            expected_unstaked
        );
        assert_eq!(
            load_ema
                .stats
                .staked_stream_load_ema
                .load(Ordering::Relaxed),
            expected_staked as usize
        );
        assert_eq!(
            load_ema
                .stats
                .unstaked_stream_load_ema
                .load(Ordering::Relaxed),
            expected_unstaked as usize
        );

        // Unstaked load alone never enables staked throttling, however high,
        // but it does count towards unstaked throttling.
        assert!(expected_unstaked >= load_ema.staked_throttling_on_load_threshold);
        assert!(expected_staked < load_ema.staked_throttling_on_load_threshold);
        assert!(!load_ema.staked_throttling_enabled.load(Ordering::Relaxed));
        assert!(
            expected_staked + expected_unstaked >= load_ema.unstaked_throttling_on_load_threshold
        );
        assert!(load_ema.unstaked_throttling_enabled.load(Ordering::Relaxed));
    }

    #[test]
    fn test_unstaked_throttling_on_total_load() {
        let mut load_ema = StreamLoadEMA::new(
            Arc::new(StreamerStats::default()),
            DEFAULT_MAX_UNSTAKED_CONNECTIONS,
            DEFAULT_MAX_STREAMS_PER_MS,
        );
        load_ema.staked_throttling_on_load_threshold = 100;
        load_ema.unstaked_throttling_on_load_threshold = 40;

        // Staked load alone above the unstaked threshold throttles unstaked
        // peers, while staked peers, still below their own threshold, are not.
        load_ema
            .staked_load_in_recent_interval
            .store(1000, Ordering::Relaxed);
        load_ema.update_ema(u128::from(STREAM_LOAD_EMA_INTERVAL_MS));
        let staked_load_ema = load_ema.staked_load_ema.load(Ordering::Relaxed);
        assert!((40..100).contains(&staked_load_ema));
        assert_eq!(load_ema.unstaked_load_ema.load(Ordering::Relaxed), 0);
        assert!(load_ema.unstaked_throttling_enabled.load(Ordering::Relaxed));
        assert!(!load_ema.staked_throttling_enabled.load(Ordering::Relaxed));
        assert_eq!(
            load_ema.available_load_capacity_in_throttling_duration(
                ConnectionPeerType::Unstaked,
                TEST_TOTAL_STAKE,
            ),
            load_ema.min_unstaked_load_in_throttling_window
        );
        assert_eq!(
            load_ema.available_load_capacity_in_throttling_duration(
                ConnectionPeerType::Staked(1),
                TEST_TOTAL_STAKE,
            ),
            load_ema.max_load_in_throttling_window
        );

        // Once staked load crosses the staked threshold, both are throttled.
        load_ema.staked_load_ema.store(200, Ordering::Relaxed);
        load_ema.update_ema(u128::from(STREAM_LOAD_EMA_INTERVAL_MS));
        assert!(load_ema.staked_throttling_enabled.load(Ordering::Relaxed));
        assert!(load_ema.unstaked_throttling_enabled.load(Ordering::Relaxed));

        // When load subsides, both are released.
        load_ema.staked_load_ema.store(0, Ordering::Relaxed);
        load_ema.unstaked_load_ema.store(0, Ordering::Relaxed);
        load_ema.update_ema(u128::from(STREAM_LOAD_EMA_INTERVAL_MS));
        assert!(!load_ema.staked_throttling_enabled.load(Ordering::Relaxed));
        assert!(!load_ema.unstaked_throttling_enabled.load(Ordering::Relaxed));
    }

    #[test]
    fn test_total_stake_zero_safety() {
        let load_ema = new_throttled_load_ema(true);

        assert_eq!(
            load_ema
                .available_load_capacity_in_throttling_duration(ConnectionPeerType::Staked(10), 0),
            load_ema.min_unstaked_load_in_throttling_window + 1
        );
    }
}
