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

#![deny(clippy::arithmetic_side_effects)]
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
        time::{Duration, Instant},
    },
    tokio::sync::{Mutex, Notify},
    tokio_util::sync::CancellationToken,
};

/// This will be added to the true stake amount in the
/// calculations to ensure that unstaked nodes have non-zero throughput
pub const BASE_STAKE_SOL: u64 = 1000;

/// This defines the minimum amount effective stake a connection
/// can hold when it is completely inactive.
const FULLY_DECAYED_STAKE_SOL: u64 = 1;

// fully decayed stake should be less than base stake
// to make sure unstaked can get properly drained of their
// bandwidth when they are not active.
const_assert!(FULLY_DECAYED_STAKE_SOL < BASE_STAKE_SOL);

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

/// Amount of tokens to gift to new connections
/// This allows fresh connections to immediately start sending
const INITIAL_TOKENS: u16 = 512;

/// Minimal size of the token bucket
const MIN_BUCKET_SIZE: u64 = 2;
const MAX_BUCKET_SIZE: u64 = 2000;
const_assert!(MAX_BUCKET_SIZE < u16::MAX as u64);

/// An abstraction to pack all of the token bucket state into one
/// AtomicU64 variable. This makes the updates easier to reason about.
#[derive(Debug, Default)]
struct BucketState {
    tokens: u16,
    consumed: u16,
    last_refill: u16,
}

#[derive(Debug)]
pub struct StreamRateLimiter {
    pub true_stake_sol: u64,
    pub effective_stake_sol: AtomicU64,
    pub address: Pubkey,
    pub bucket_state: AtomicU64, // actually stores BucketState
    wake_notify: Notify,
}

impl StreamRateLimiter {
    pub fn new(address: Pubkey, stake_lamports: u64) -> Self {
        let stake_sol = (stake_lamports / LAMPORTS_PER_SOL).max(BASE_STAKE_SOL);
        Self {
            true_stake_sol: stake_sol,
            effective_stake_sol: AtomicU64::new(stake_sol),
            bucket_state: AtomicU64::new(
                BucketState {
                    tokens: INITIAL_TOKENS,
                    consumed: 0,
                    last_refill: INITIAL_TOKENS,
                }
                .into(),
            ),
            address,
            wake_notify: Notify::new(),
        }
    }

    pub fn new_unstaked() -> Self {
        Self::new(Pubkey::new_unique(), 0)
    }

    #[cfg(test)]
    fn tokens(&self) -> u64 {
        BucketState::from(self.bucket_state.load(Ordering::Relaxed)).tokens as u64
    }

    /// try to consume a token from the throttler, if it can not it will block.
    pub async fn wait_for_token(&self, stats: &StreamerStats) {
        while self
            .bucket_state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| {
                let BucketState {
                    tokens,
                    consumed,
                    last_refill,
                } = BucketState::from(v);
                if tokens == 0 {
                    None
                } else {
                    Some(
                        BucketState {
                            tokens: tokens.saturating_sub(1),
                            consumed: consumed.saturating_add(1),
                            last_refill,
                        }
                        .into(),
                    )
                }
            })
            .is_err()
        {
            let t0 = Instant::now();
            self.wake_notify.notified().await;
            let throttled_ms = t0.elapsed().as_millis() as u64;
            trace!(
                "Throttled connection from {} for {} ms",
                self.address,
                throttled_ms
            );
            if self.true_stake_sol <= BASE_STAKE_SOL {
                stats
                    .throttled_ms_unstaked
                    .fetch_add(throttled_ms, Ordering::Relaxed);
            } else {
                stats
                    .throttled_ms_staked
                    .fetch_add(throttled_ms, Ordering::Relaxed);
            }
            stats
                .throttled_time_ms
                .fetch_add(throttled_ms, Ordering::Relaxed);
        }
    }

    /// A helper to drain the bucket as if it was actually used via repeated calls
    /// to `wait_for_token`. This is not safe to call in a multithreaded context.
    #[cfg(test)]
    fn drain(&self) -> u64 {
        let BucketState {
            tokens,
            consumed: _,
            last_refill,
        } = BucketState::from(self.bucket_state.load(Ordering::SeqCst));

        self.bucket_state.store(
            BucketState {
                tokens: 0,
                consumed: tokens,
                last_refill,
            }
            .into(),
            Ordering::SeqCst,
        );
        tokens as u64
    }

    /// Refill the token bucket. Updates the effective stake internally
    /// and returns the new value.
    pub fn refill(&self, refill_amount: u16, my_max_tokens: u16) -> u64 {
        debug_assert!(
            refill_amount > 0,
            "Refill should never be zero to ensure stake decay works"
        );
        let mut consumed_snapshot = 0;
        let mut last_refill_snapshot = 0;
        self.bucket_state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| {
                let BucketState {
                    tokens,
                    consumed,
                    last_refill,
                } = BucketState::from(v);

                consumed_snapshot = consumed;
                last_refill_snapshot = last_refill;

                let new_tokens = (tokens.saturating_add(refill_amount)).min(my_max_tokens);

                Some(
                    BucketState {
                        tokens: new_tokens,
                        consumed: 0,
                        last_refill: refill_amount,
                    }
                    .into(),
                )
            })
            .unwrap();

        self.wake_notify.notify_waiters();
        // compute effective stake based on the utiliation of last refill
        let effective_stake = self
            .true_stake_sol
            .saturating_mul(consumed_snapshot as u64)
            // last_refill would be zero immediately after creation of the connection
            .checked_div(last_refill_snapshot as u64)
            .unwrap_or(self.true_stake_sol)
            .clamp(FULLY_DECAYED_STAKE_SOL, self.true_stake_sol);

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
    max_tps
        .saturating_mul(REFILL_INTERVAL.as_millis() as u64)
        .saturating_div(1000)
}

pub async fn refill_task(
    staked_connection_table: Arc<Mutex<ConnectionTable<StreamRateLimiter>>>,
    unstaked_connection_table: Arc<Mutex<ConnectionTable<StreamRateLimiter>>>,
    max_tps: u64,
    cancel: CancellationToken,
) {
    debug!("Spawning refill task with {max_tps} TPS");

    let mut interval = tokio::time::interval(REFILL_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
    let mut refiller = Refiller::new(staked_connection_table, unstaked_connection_table).await;
    while !cancel.is_cancelled() {
        interval.tick().await; // first tick completes instantly
        refiller.do_refill(max_tps).await;
    }
}

pub struct Refiller {
    last_iter_total_stake: u64,
    last_iter_effective_stake: u64,
    staked_connection_table: Arc<Mutex<ConnectionTable<StreamRateLimiter>>>,
    unstaked_connection_table: Arc<Mutex<ConnectionTable<StreamRateLimiter>>>,
}

impl Refiller {
    pub async fn new(
        staked_connection_table: Arc<Mutex<ConnectionTable<StreamRateLimiter>>>,
        unstaked_connection_table: Arc<Mutex<ConnectionTable<StreamRateLimiter>>>,
    ) -> Self {
        let last_iter_total_stake = {
            // initialize effective stake of unstaked connections based on their number
            let guard = unstaked_connection_table.lock().await;
            (guard.table_size() as u64).saturating_mul(BASE_STAKE_SOL)
        }
        .saturating_add({
            // and for staked use actual stake
            let guard = staked_connection_table.lock().await;
            guard.connected_stake() / LAMPORTS_PER_SOL
        });

        Self {
            last_iter_total_stake,
            last_iter_effective_stake: last_iter_total_stake,
            staked_connection_table,
            unstaked_connection_table,
        }
    }

    ///Refill the rate limiters in connection tables
    #[allow(clippy::arithmetic_side_effects)]
    pub async fn do_refill(&mut self, max_tps: u64) -> u64 {
        // counts allocated tokens for debugging
        let mut allocated_tokens = 0;
        // retrieve stats from last iteration, make sure we do not get zero in
        // the total counters to avoid division by zero.
        let total_effective_stake = self.last_iter_effective_stake.max(BASE_STAKE_SOL);
        self.last_iter_effective_stake = 0;
        let total_stake = self.last_iter_total_stake.max(BASE_STAKE_SOL);
        self.last_iter_total_stake = 0;
        let token_fill_per_interval = token_fill_per_interval(max_tps);

        for conn_table in [
            &self.staked_connection_table,
            &self.unstaked_connection_table,
        ] {
            let guard = conn_table.lock().await;
            for (_key, connection_entry_vec) in guard.iter() {
                let Some(connection_entry) = connection_entry_vec.first() else {
                    continue;
                };
                let entry = connection_entry.stream_counter.as_ref();
                let entry_effective_stake = entry.effective_stake();

                // minimal amount this bucket should be able to hold (proportional to stake)
                // this allows high-staked connections to ramp up faster
                let token_capacity_min =
                    (token_fill_per_interval * MAX_BURST * entry.true_stake_sol / total_stake)
                        .clamp(MIN_BUCKET_SIZE, MAX_BUCKET_SIZE);
                // share of total amount to deposit in this token bucket
                let my_token_share =
                    token_fill_per_interval * entry_effective_stake / total_effective_stake;
                // make sure we always deposit some tokens
                let my_token_share = my_token_share.clamp(1, u16::MAX as u64);
                // maximal amount this bucket should be able to hold (proportional to effective stake)
                let tokens_capacity_max =
                    (my_token_share * MAX_BURST).clamp(token_capacity_min, MAX_BUCKET_SIZE);
                trace!(
                    "Grant {my_token_share} (max {tokens_capacity_max}) TXs to {} based on \
                     {}/{total_effective_stake} sol of stake.",
                    entry.address,
                    entry.effective_stake()
                );
                allocated_tokens += my_token_share;
                // fill the bucket with all available tokens
                // record effective stake of the entry after refill to keep track
                // of state as connections come and go
                self.last_iter_effective_stake +=
                    entry.refill(my_token_share as u16, tokens_capacity_max as u16);

                self.last_iter_total_stake += entry.true_stake_sol;
            }
        }
        trace!(
            "Allocated {allocated_tokens} tokens out of {token_fill_per_interval} to users. \
             total_effective_stake={total_effective_stake}, total_stake={total_stake}"
        );
        allocated_tokens
    }
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

// these conversion helpers optimize to
// the same assembly as if we used transmute, minus
// the unsafe block.
impl From<u64> for BucketState {
    #[inline]
    fn from(v: u64) -> Self {
        Self {
            tokens: v as u16,
            consumed: (v >> 16) as u16,
            last_refill: (v >> 32) as u16,
        }
    }
}

impl From<BucketState> for u64 {
    #[inline]
    fn from(val: BucketState) -> Self {
        (val.tokens as u64) | ((val.consumed as u64) << 16) | ((val.last_refill as u64) << 32)
    }
}
#[cfg(test)]
pub mod test {
    #![allow(clippy::arithmetic_side_effects)]
    use {
        crate::{
            nonblocking::{
                stream_throttle::{
                    token_fill_per_interval, Refiller, StreamRateLimiter, BASE_STAKE_SOL,
                    FULLY_DECAYED_STAKE_SOL, INITIAL_TOKENS, REFILL_INTERVAL,
                },
                testing_utilities::fill_connection_table,
            },
            quic::StreamerStats,
        },
        solana_native_token::LAMPORTS_PER_SOL,
        solana_pubkey::Pubkey,
        std::{
            future::Future,
            net::{IpAddr, Ipv4Addr, SocketAddr},
            sync::Arc,
            time::{Duration, Instant},
        },
        tokio::time::timeout,
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
        entry.drain();
        assert_eq!(
            entry.refill(max_tokens, max_tokens),
            BASE_STAKE_SOL,
            "Should have full stake applied"
        );
        assert_eq!(
            entry.refill(max_tokens, max_tokens),
            FULLY_DECAYED_STAKE_SOL,
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

    const MAX_TPS: u64 = 10000;
    const NUM_CLIENTS: usize = 10;
    const TOKEN_FILL_PER_INTERVAL: u64 = token_fill_per_interval(MAX_TPS);

    /// Test that the Refiller is working correctly:
    /// * if we use the tokens they get reflled
    /// * if we do not use the tokens effective stake decays
    /// * once we start using the effective stake returns
    #[tokio::test]
    async fn test_refiller_stake_decay() {
        agave_logger::setup();
        let stats = Arc::new(StreamerStats::default());
        let cancel = CancellationToken::new();

        let staked_connection_table = fill_connection_table(&[], &[], stats.clone());

        let sockets: Vec<_> = (0..NUM_CLIENTS as u32)
            .map(|i| SocketAddr::new(IpAddr::V4(Ipv4Addr::from_bits(i)), 0))
            .collect();

        let rate_limiters: Vec<_> = sockets
            .iter()
            .map(|_| Arc::new(StreamRateLimiter::new_unstaked()))
            .collect();

        let unstaked_connection_table =
            fill_connection_table(&sockets, &rate_limiters, stats.clone());

        let mut refiller = Refiller::new(staked_connection_table, unstaked_connection_table).await;
        assert_eq!(
            rate_limiters[0].tokens(),
            INITIAL_TOKENS as u64,
            "Should have INITIAL_TOKENS initial tokens"
        );

        for rl in rate_limiters.iter() {
            rl.drain();
        }
        assert_eq!(rate_limiters[0].tokens(), 0, "Should have no tokens");
        assert_blocks(
            rate_limiters[0].wait_for_token(&stats),
            "wait on empty buckets",
        )
        .await;

        info!("wait for token after refill");
        refiller.do_refill(MAX_TPS).await;
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

        info!("refill twice without consuming");
        refiller.do_refill(MAX_TPS).await;
        refiller.do_refill(MAX_TPS).await;
        assert_eq!(
            rate_limiters[0].effective_stake(),
            FULLY_DECAYED_STAKE_SOL,
            "Should have all stake retracted due to no use during last refill"
        );
        info!("test under max consumption");
        rate_limiters[0].drain();
        refiller.do_refill(MAX_TPS).await;
        rate_limiters[0].drain();
        refiller.do_refill(MAX_TPS).await;

        let effective_stake = (NUM_CLIENTS as u64 - 1) * FULLY_DECAYED_STAKE_SOL + BASE_STAKE_SOL;
        let expected_tokens = TOKEN_FILL_PER_INTERVAL * BASE_STAKE_SOL / effective_stake;
        assert_eq!(
            rate_limiters[0].tokens(),
            expected_tokens,
            "Bucket should be filled now"
        );

        info!("test under no consumption");
        refiller.do_refill(MAX_TPS).await;
        refiller.do_refill(MAX_TPS).await;
        assert_eq!(
            rate_limiters[0].effective_stake(),
            FULLY_DECAYED_STAKE_SOL,
            "Should have reduced effective stake due to lack of consumption"
        );
        cancel.cancel();
    }

    /// Test the SWQOS - that token allocations are stake proportional under load
    #[tokio::test]
    async fn test_refiller_swqos() {
        agave_logger::setup();
        let stats = Arc::new(StreamerStats::default());

        const NUM_UNSTAKED_CLIENTS: usize = NUM_CLIENTS / 2;
        let sockets: Vec<_> = (0..NUM_CLIENTS as u32)
            .map(|i| SocketAddr::new(IpAddr::V4(Ipv4Addr::from_bits(i)), 0))
            .collect();
        let (unstaked_sockets, staked_sockets) = sockets.split_at(NUM_UNSTAKED_CLIENTS);
        let unstaked_rate_limiters: Vec<_> = unstaked_sockets
            .iter()
            .map(|_| Arc::new(StreamRateLimiter::new_unstaked()))
            .collect();
        let staked_rate_limiters: Vec<_> = staked_sockets
            .iter()
            .enumerate()
            .map(|(index, _)| {
                Arc::new(StreamRateLimiter::new(
                    Pubkey::new_unique(),
                    BASE_STAKE_SOL * LAMPORTS_PER_SOL * (index + 1) as u64,
                ))
            })
            .collect();

        let unstaked_connection_table =
            fill_connection_table(unstaked_sockets, &unstaked_rate_limiters, stats.clone());
        let staked_connection_table =
            fill_connection_table(staked_sockets, &staked_rate_limiters, stats.clone());

        let all_rate_limiters: Vec<_> = unstaked_rate_limiters
            .iter()
            .cloned()
            .chain(staked_rate_limiters.iter().cloned())
            .collect();

        let mut refiller = Refiller::new(staked_connection_table, unstaked_connection_table).await;
        assert_eq!(
            all_rate_limiters[0].tokens(),
            INITIAL_TOKENS as u64,
            "Should have INITIAL_TOKENS initial tokens"
        );

        for rl in all_rate_limiters.iter() {
            rl.drain();
        }
        assert_eq!(all_rate_limiters[0].tokens(), 0, "Should have no tokens");

        let fake_stake = NUM_UNSTAKED_CLIENTS as u64 * BASE_STAKE_SOL;
        let real_stake = refiller
            .staked_connection_table
            .lock()
            .await
            .connected_stake()
            / LAMPORTS_PER_SOL;
        let total_stake = fake_stake + real_stake;
        assert_eq!(
            refiller.last_iter_total_stake, total_stake,
            "total stake observed by refill
            should match actual"
        );

        info!("fill everything initially");
        refiller.do_refill(MAX_TPS).await;

        let allocations: Vec<_> = all_rate_limiters.iter().map(|rl| rl.drain()).collect();
        debug!("{allocations:?}");
        assert_eq!(
            allocations.iter().sum::<u64>(),
            TOKEN_FILL_PER_INTERVAL,
            "Should have allocated all tokens"
        );

        for alloc in &allocations[..NUM_UNSTAKED_CLIENTS] {
            assert_eq!(
                *alloc,
                TOKEN_FILL_PER_INTERVAL * fake_stake / total_stake / NUM_UNSTAKED_CLIENTS as u64
            )
        }
        let staked_refill = TOKEN_FILL_PER_INTERVAL * real_stake / total_stake;
        for (alloc, rl) in allocations[NUM_UNSTAKED_CLIENTS..]
            .iter()
            .zip(staked_rate_limiters.iter())
        {
            assert_eq!(
                *alloc,
                staked_refill * rl.true_stake_sol / real_stake as u64
            )
        }
        info!("Staked nodes do not use bandwidth");
        // ensure effective stake decay by draining only staked stream limiters
        refiller.do_refill(MAX_TPS).await;
        let _allocations: Vec<_> = unstaked_rate_limiters.iter().map(|rl| rl.drain()).collect();
        refiller.do_refill(MAX_TPS).await;
        let _allocations: Vec<_> = unstaked_rate_limiters.iter().map(|rl| rl.drain()).collect();
        refiller.do_refill(MAX_TPS).await;
        let total_effective_stake = refiller.last_iter_effective_stake;
        let allocations: Vec<_> = unstaked_rate_limiters.iter().map(|rl| rl.drain()).collect();
        debug!("{allocations:?}");
        for alloc in allocations {
            assert_eq!(
                alloc,
                TOKEN_FILL_PER_INTERVAL * fake_stake
                    / total_effective_stake
                    / NUM_UNSTAKED_CLIENTS as u64,
                "Unstaked should be getting more tokens now that staked are idle"
            )
        }
        info!("Untaked nodes do not use bandwidth");

        // ensure effective stake decay by draining only staked stream limiters
        refiller.do_refill(MAX_TPS).await;
        let _allocations: Vec<_> = staked_rate_limiters.iter().map(|rl| rl.drain()).collect();
        refiller.do_refill(MAX_TPS).await;
        let _allocations: Vec<_> = staked_rate_limiters.iter().map(|rl| rl.drain()).collect();
        refiller.do_refill(MAX_TPS).await;
        let total_effective_stake = refiller.last_iter_effective_stake;
        let allocations: Vec<_> = staked_rate_limiters.iter().map(|rl| rl.drain()).collect();
        debug!("{allocations:?}");
        let staked_refill = TOKEN_FILL_PER_INTERVAL * real_stake / total_effective_stake;
        for (alloc, rl) in allocations.iter().zip(staked_rate_limiters.iter()) {
            assert_eq!(
                *alloc,
                staked_refill * rl.true_stake_sol / real_stake as u64
            )
        }
    }

    /// Awaits `fut` for 100ms.
    /// - If the future **completes within 100ms**, this function panics.
    /// - If the future **does not complete within 100ms**, this function returns normally.
    pub async fn assert_blocks<F>(fut: F, msg: &'static str)
    where
        F: Future,
    {
        match timeout(Duration::from_millis(100), fut).await {
            Ok(_) => panic!("{msg} did not block"),
            Err(_) => {
                // timed out as expected; do nothing
            }
        }
    }
}
