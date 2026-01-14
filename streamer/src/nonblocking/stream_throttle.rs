//! This module implements the stake-weighted read throttling logic:
//! * Each connected client is assigned a fraction of total TPS budget R
//!   in proportion to its stake.
//! * All unused capacity gets added to R for the next round of allocation
//! * Unstaked nodes get granted fake stake as if they were staked
//!
//! To better utilize the bandwidth, the effective usage rate is
//! estimated based on how much they have consumed last round, and refill
//! proportion is based on the total usage rate, not true stake.
//! This allows to efficiently reassign underutilized bandwidth to other
//! users, as their effective stake in the overall allocation grows.

use {
    crate::{
        nonblocking::{qos::OpaqueStreamerCounter, quic::ConnectionTable},
        quic::StreamerStats,
    },
    solana_native_token::LAMPORTS_PER_SOL,
    solana_pubkey::Pubkey,
    static_assertions::const_assert,
    std::{
        cmp,
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc,
        },
        time::Duration,
    },
    tokio::{sync::Mutex, time::sleep},
    tokio_util::sync::CancellationToken,
};

/// This will be added to the true stake amount in the
/// calculations to ensure that unstaked nodes have non-zero throughput
pub const BASE_STAKE_SOL: u64 = 1000;

/// This is a divisor that defines how much stake can a connection
/// "lose" due to inactivity. The higher it is, the more unused
/// bandwidth will get redistributed to other connections.
const STAKE_LOSS_MAX_FRACTION: u64 = 50;

// Stake loss fraction must be low enough to ensure everyone has non-zero
// stake as maximal loss is applied
const_assert!(BASE_STAKE_SOL / STAKE_LOSS_MAX_FRACTION > 0);

/// Interval of refills for the QoS token buckets
pub const REFILL_INTERVAL: Duration = Duration::from_millis(20);

/// How many [`REFILL_INTERVAL`] worth of token refill rate do we accumulate
/// for idle connections (to handle bursts of arrivals).
///
/// For example, given `REFILL_INTERVAL = 100ms` and `MAX_BURST = 3`  we can
///  * sustain 4x rate for 100ms or
///  * sustain 2x rate for 200ms
///
/// `MAX_BURST = 1` disables the feature
const MAX_BURST: u64 = 2;

/// Minimal size of the token bucket
const MIN_BUCKET_SIZE: u64 = 2;
const MAX_BUCKET_SIZE: u64 = 2000;

#[derive(Debug)]
pub struct StreamRateLimiter {
    pub true_stake_sol: u64,
    pub effective_stake_sol: AtomicU64,
    pub number_of_times_throttled: AtomicU64,
    pub address: Pubkey,
    pub tokens: AtomicU64,
    pub consumed_tokens: AtomicU64,
    pub last_refill: AtomicU64,
}
impl OpaqueStreamerCounter for StreamRateLimiter {}

impl Ord for StreamRateLimiter {
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        // high stake comes first
        other.true_stake_sol.cmp(&self.true_stake_sol)
    }
}
impl PartialOrd for StreamRateLimiter {
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for StreamRateLimiter {
    fn eq(&self, other: &Self) -> bool {
        self.true_stake_sol == other.true_stake_sol
    }
}
impl Eq for StreamRateLimiter {}

impl StreamRateLimiter {
    pub fn new(address: Pubkey, stake_lamports: u64) -> Self {
        let stake_sol = stake_lamports / LAMPORTS_PER_SOL + BASE_STAKE_SOL;
        Self {
            true_stake_sol: stake_sol,
            effective_stake_sol: AtomicU64::new(stake_sol),
            tokens: AtomicU64::new(0),
            consumed_tokens: AtomicU64::new(0),
            last_refill: AtomicU64::new(0),
            address,
            number_of_times_throttled: AtomicU64::new(0),
        }
    }

    pub fn new_unstaked() -> Self {
        Self::new(Pubkey::new_unique(), 0)
    }

    #[cfg(test)]
    fn tokens(&self) -> u64 {
        self.tokens.load(Ordering::Relaxed)
    }

    /// try to consume a token from the throttler, if it can not it will block.
    pub async fn wait_for_token(&self, stats: &StreamerStats) {
        while self
            .tokens
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| {
                if v == 0 {
                    None
                } else {
                    Some(v - 1)
                }
            })
            .is_err()
        {
            debug!(
                "Throttling connection from {} for {REFILL_INTERVAL:?}",
                self.address
            );
            self.number_of_times_throttled
                .fetch_add(1, Ordering::Relaxed);
            stats.throttled_streams.fetch_add(1, Ordering::Relaxed);
            if self.true_stake_sol == 0 {
                stats
                    .throttled_unstaked_streams
                    .fetch_add(1, Ordering::Relaxed);
            } else {
                stats
                    .throttled_staked_streams
                    .fetch_add(1, Ordering::Relaxed);
            }

            sleep(REFILL_INTERVAL / 2).await;
        }
        self.consumed_tokens.fetch_add(1, Ordering::Relaxed);
    }

    #[cfg(test)]
    fn drain(&self) {
        let old = self.tokens.swap(0, Ordering::Relaxed);
        self.consumed_tokens.store(old, Ordering::Relaxed);
    }

    /// Refill the token bucket. Updates the effective stake internally
    /// and returns the new value.
    pub fn refill(&self, refill_amount: u64, my_max_tokens: u64) -> u64 {
        let consumed = self.consumed_tokens.swap(0, Ordering::Relaxed);
        let last_refill = self.last_refill.swap(refill_amount, Ordering::Relaxed);
        let current = self.tokens.load(Ordering::Relaxed);
        let _previous = self.tokens.fetch_max(
            my_max_tokens.min(current + refill_amount),
            Ordering::Relaxed,
        );

        // compute effective stake based on the utiliation of last refill
        let effective_stake = if last_refill > 0 {
            self.true_stake_sol * consumed / last_refill
        } else {
            self.true_stake_sol
        }
        .clamp(
            self.true_stake_sol / STAKE_LOSS_MAX_FRACTION,
            self.true_stake_sol,
        );

        debug_assert!(effective_stake > 0, "effective_stake should not be zero");
        self.effective_stake_sol
            .store(effective_stake, Ordering::Relaxed);
        effective_stake
    }

    #[inline]
    fn effective_stake(&self) -> u64 {
        self.effective_stake_sol.load(Ordering::Relaxed)
    }
}

const fn token_fill_per_interval(max_tps: u64) -> u64 {
    max_tps * REFILL_INTERVAL.as_millis() as u64 / 1000
}

#[allow(clippy::arithmetic_side_effects)]
pub async fn refill_task(
    staked_connection_table: Arc<Mutex<ConnectionTable<StreamRateLimiter>>>,
    unstaked_connection_table: Arc<Mutex<ConnectionTable<StreamRateLimiter>>>,
    max_tps: u64,
    cancel: CancellationToken,
) {
    debug!("Spawning refill task with {max_tps} TPS");
    let mut last_iter_total_stake = {
        // initialize effective stake of unstaked connections based on their number
        let guard = unstaked_connection_table.lock().await;
        guard.table_size() as u64 * BASE_STAKE_SOL
    } + {
        // and for staked use actual stake
        let guard = staked_connection_table.lock().await;
        guard.connected_stake() / LAMPORTS_PER_SOL
    };
    let mut last_iter_effective_stake = last_iter_total_stake;

    let mut interval = tokio::time::interval(REFILL_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
    let token_fill_per_interval = token_fill_per_interval(max_tps);
    while !cancel.is_cancelled() {
        interval.tick().await; // first tick completes instantly

        // retrieve stats from last iteration, make sure we do not get zero in
        // the total counters to avoid division by zero.
        let total_effective_stake = last_iter_effective_stake.max(BASE_STAKE_SOL);
        last_iter_effective_stake = 0;
        let total_stake = last_iter_total_stake.max(BASE_STAKE_SOL);
        last_iter_total_stake = 0;

        // count allocated tokens for debugging
        let mut allocated_tokens = 0;

        // actually fill all the buckets
        for conn_table in [&staked_connection_table, &unstaked_connection_table] {
            let guard = conn_table.lock().await;
            for (_key, connection_entry_vec) in guard.iter() {
                let Some(connection_entry) = connection_entry_vec.first() else {
                    continue;
                };
                let entry = connection_entry.stream_counter.as_ref();
                let entry_effective_stake = entry.effective_stake();

                // minimal amount this bucket should be able to hold (proportional to stake)
                let my_min_tokens = (token_fill_per_interval * MAX_BURST * entry.true_stake_sol
                    / total_stake)
                    .clamp(MIN_BUCKET_SIZE, MAX_BUCKET_SIZE);
                // share of total amount to deposit in this token bucket
                let my_token_share =
                    token_fill_per_interval * entry_effective_stake / total_effective_stake;
                // maximal amount this bucket should be able to hold (proportional to consumption)
                let my_max_tokens =
                    (my_token_share * MAX_BURST).clamp(my_min_tokens, MAX_BUCKET_SIZE);
                trace!(
                    "Grant {my_token_share} (max {my_max_tokens}) TXs to {} based on \
                     {}/{total_effective_stake} sol of stake.",
                    entry.address,
                    entry.effective_stake()
                );
                allocated_tokens += my_token_share;
                // fill the bucket with all available tokens
                // record effective stake of the entry after refill
                last_iter_effective_stake += entry.refill(my_token_share, my_max_tokens);
                last_iter_total_stake += entry.true_stake_sol;
            }
        }
        debug!(
            "Allocated {allocated_tokens} tokens out of {token_fill_per_interval} to users. \
             total_effective_stake={total_effective_stake}, total_stake={total_stake}"
        );
    }
}

#[cfg(test)]
pub mod test {
    use {
        crate::{
            nonblocking::{
                quic::{
                    ClientConnectionTracker, ConnectionPeerType, ConnectionTable,
                    ConnectionTableKey, ConnectionTableType,
                },
                stream_throttle::{
                    refill_task, token_fill_per_interval, StreamRateLimiter, BASE_STAKE_SOL,
                    MAX_BUCKET_SIZE, REFILL_INTERVAL, STAKE_LOSS_MAX_FRACTION,
                },
            },
            quic::StreamerStats,
        },
        std::{
            net::{IpAddr, Ipv4Addr, SocketAddr},
            sync::{atomic::AtomicU64, Arc},
            time::Instant,
        },
        tokio::sync::Mutex,
        tokio_util::sync::CancellationToken,
    };

    #[tokio::test]
    async fn test_stream_rate_limiter() {
        let max_tokens = 100;
        let entry = StreamRateLimiter::new_unstaked();
        assert_eq!(
            entry.effective_stake(),
            BASE_STAKE_SOL,
            "Unstaked nodes should be given BASE_STAKE_SOL worth of stake"
        );
        assert_eq!(
            entry.refill(max_tokens, max_tokens),
            BASE_STAKE_SOL,
            "Should have full stake applied"
        );
        assert_eq!(
            entry.refill(max_tokens, max_tokens),
            BASE_STAKE_SOL / STAKE_LOSS_MAX_FRACTION,
            "Should have lost all possible stake due to no usage"
        );

        let stats = StreamerStats::default();
        entry.drain();
        assert_eq!(
            entry.refill(max_tokens, max_tokens),
            BASE_STAKE_SOL,
            "Should have full stake reapplied"
        );
        let t0 = Instant::now();
        for _ in 0..100 {
            entry.wait_for_token(&stats).await;
        }
        assert!(t0.elapsed() < REFILL_INTERVAL, "should not be blocked");
        let to = tokio::time::timeout(REFILL_INTERVAL, entry.wait_for_token(&stats)).await;
        assert!(to.is_err(), "Must block waiting on token arrival");
    }

    #[tokio::test]
    async fn test_refiller() {
        agave_logger::setup();
        let cancel = CancellationToken::new();

        let unstaked_connection_table: Arc<Mutex<ConnectionTable<StreamRateLimiter>>> =
            Arc::new(Mutex::new(ConnectionTable::new(
                ConnectionTableType::Unstaked,
                cancel.clone(),
            )));
        let staked_connection_table: Arc<Mutex<ConnectionTable<StreamRateLimiter>>> =
            Arc::new(Mutex::new(ConnectionTable::new(
                ConnectionTableType::Staked,
                cancel.clone(),
            )));
        const MAX_TPS: u64 = 1000000;
        const NUM_CLIENTS: u32 = 1000;
        const TOKEN_FILL_PER_INTERVAL: u64 = token_fill_per_interval(MAX_TPS);
        let max_connections_per_peer = 1;
        let sockets: Vec<_> = (0..NUM_CLIENTS)
            .map(|i| SocketAddr::new(IpAddr::V4(Ipv4Addr::from_bits(i)), 0))
            .collect();
        let stats = Arc::new(StreamerStats::default());
        let rate_limiters: Vec<_> = sockets
            .iter()
            .map(|_| Arc::new(StreamRateLimiter::new_unstaked()))
            .collect();

        for (socket, limiter) in sockets.iter().zip(rate_limiters.iter().cloned()) {
            unstaked_connection_table
                .lock()
                .await
                .try_add_connection(
                    ConnectionTableKey::IP(socket.ip()),
                    socket.port(),
                    ClientConnectionTracker::new(stats.clone(), NUM_CLIENTS as usize).unwrap(),
                    None,
                    ConnectionPeerType::Unstaked,
                    Arc::new(AtomicU64::new(10)),
                    max_connections_per_peer,
                    || limiter,
                )
                .unwrap();
        }

        assert_eq!(
            rate_limiters[0].tokens(),
            0,
            "Should have no initial tokens"
        );

        let refiller = tokio::spawn(refill_task(
            staked_connection_table,
            unstaked_connection_table,
            MAX_TPS,
            cancel.clone(),
        ));
        rate_limiters[0].wait_for_token(&stats).await;
        assert_eq!(
            rate_limiters[0].tokens(),
            (TOKEN_FILL_PER_INTERVAL / NUM_CLIENTS as u64 - 1),
            "Should have spent 1 transaction for the one we just consumed"
        );
        assert_eq!(
            rate_limiters[0].effective_stake(),
            BASE_STAKE_SOL,
            "Should have all stake applied"
        );
        tokio::time::sleep(REFILL_INTERVAL * 2).await;
        assert_eq!(
            rate_limiters[0].effective_stake(),
            BASE_STAKE_SOL / STAKE_LOSS_MAX_FRACTION,
            "Should have all stake retracted due to no use during last refill"
        );
        info!("drain one bucket");
        rate_limiters[0].drain();
        rate_limiters[0].wait_for_token(&stats).await;
        info!("wait for buckets to fill up again");
        rate_limiters[0].drain();
        rate_limiters[0].wait_for_token(&stats).await;

        let effective_stake =
            (NUM_CLIENTS as u64 - 1) * BASE_STAKE_SOL / STAKE_LOSS_MAX_FRACTION + BASE_STAKE_SOL;
        let expected_tokens = TOKEN_FILL_PER_INTERVAL * BASE_STAKE_SOL / effective_stake;
        assert_eq!(
            rate_limiters[0].tokens(),
            expected_tokens - 1,
            "Bucket should be filled now -1 transaction we just used"
        );
        tokio::time::sleep(REFILL_INTERVAL * 2).await;
        assert_eq!(
            rate_limiters[0].effective_stake(),
            BASE_STAKE_SOL / STAKE_LOSS_MAX_FRACTION,
            "Should have stake reduced due to lack of consumption"
        );
        info!("drain a single bucket consistently");
        for _ in 0..MAX_BUCKET_SIZE {
            rate_limiters[0].wait_for_token(&stats).await;
        }
        assert_eq!(
            rate_limiters[0].effective_stake(),
            BASE_STAKE_SOL,
            "Should have full stake due to full consumption"
        );
        cancel.cancel();
        refiller.await.unwrap();
    }
}
