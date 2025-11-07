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
        nonblocking::{
            qos::{get_shared_state, ConnectionTableSharedState},
            quic::ConnectionTable,
        },
        quic::StreamerStats,
        streamer::StakedNodes,
    },
    solana_native_token::LAMPORTS_PER_SOL,
    solana_pubkey::Pubkey,
    std::{
        any::Any,
        cmp,
        collections::HashMap,
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc,
        },
        time::Duration,
    },
    tokio::{
        sync::{watch, Mutex},
        time::sleep,
    },
};

/// This will be added to the true stake amount in the
/// calculations to ensure that unstaked nodes have non-zero throughput
pub const BASE_STAKE_SOL: u64 = 1000;

/// This is a divisor that defines how much stake can a connection
/// "lose" due to inactivity. The higher it is, the more unused
/// bandwidth will get redistributed to other connections.
const STAKE_LOSS_MAX_FRACTION: u64 = 20;

/// Interval of refills for the QoS token buckets
pub const REFILL_INTERVAL: Duration = Duration::from_millis(10);

/// How many [`REFILL_INTERVAL`] worth of token refill rate do we accumulate
/// for idle connections (to handle bursts of arrivals).
///
/// For example, given `REFILL_INTERVAL = 100ms` and `MAX_BURST = 3`  we can
///  * sustain 4x rate for 100ms or
///  * sustain 2x rate for 200ms
///
/// `MAX_BURST = 1` disables the feature
const MAX_BURST: u64 = 2;

#[derive(Clone)]
pub struct StakedStreamQuotas {
    pub entries: HashMap<Pubkey, Arc<QuotaEntry>>,
    pub total_effective_stake: u64,
}

pub struct ConnectionStreamCounter {
    pub quota: Arc<QuotaEntry>,
}

impl ConnectionStreamCounter {
    pub fn new(quota: Arc<QuotaEntry>) -> Self {
        Self { quota }
    }
    pub fn new_unstaked() -> Self {
        Self {
            quota: Arc::new(QuotaEntry::new(Pubkey::new_unique(), 0)),
        }
    }
}
impl ConnectionTableSharedState for ConnectionStreamCounter {
    fn as_any_arc(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self.clone()
    }
}

#[derive(Debug)]
pub struct QuotaEntry {
    pub true_stake_sol: u64,
    pub effective_stake_sol: AtomicU64,
    pub number_of_times_throttled: AtomicU64,
    pub address: Pubkey,
    pub tokens: AtomicU64,
}

impl Ord for QuotaEntry {
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        // high stake comes first
        other.true_stake_sol.cmp(&self.true_stake_sol)
    }
}
impl PartialOrd for QuotaEntry {
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for QuotaEntry {
    fn eq(&self, other: &Self) -> bool {
        self.true_stake_sol == other.true_stake_sol
    }
}
impl Eq for QuotaEntry {}

impl QuotaEntry {
    pub fn new(address: Pubkey, stake_lamports: u64) -> Self {
        let stake_sol = stake_lamports / LAMPORTS_PER_SOL + BASE_STAKE_SOL;
        Self {
            true_stake_sol: stake_sol,
            effective_stake_sol: AtomicU64::new(stake_sol),
            tokens: AtomicU64::new(128),
            address,
            number_of_times_throttled: AtomicU64::new(0),
        }
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

            sleep(REFILL_INTERVAL).await;
        }
    }

    /// Refill the token bucket. Updates the effective stake internally
    /// and returns the new value.
    pub fn refill(&self, refill_amount: u64, my_max_tokens: u64) -> u64 {
        let current = self.tokens.load(Ordering::Relaxed);
        // this is technically a race, but since the other threads can only
        // decrement the token count, we are guaranteed to never allocate too many tokens
        let previous = self.tokens.fetch_max(
            my_max_tokens.min(current + refill_amount),
            Ordering::Relaxed,
        );

        let overflow = (previous + refill_amount).saturating_sub(my_max_tokens);

        // compute how much of our refill did we actually utilize, and compute
        // our stake scaled by the utiliation

        let scaled_stake = if refill_amount > 0 {
            self.effective_stake() * refill_amount.saturating_sub(overflow) / refill_amount
        } else {
            self.true_stake_sol
        };
        let effective_stake = scaled_stake.max(self.true_stake_sol / STAKE_LOSS_MAX_FRACTION);
        self.effective_stake_sol
            .store(effective_stake, Ordering::Relaxed);
        // return scaled stake, but never return zero to avoid arithmetic errors
        effective_stake
    }

    #[inline]
    fn effective_stake(&self) -> u64 {
        self.effective_stake_sol.load(Ordering::Relaxed)
    }
}

impl StakedStreamQuotas {
    pub fn new(stakes: &StakedNodes) -> Self {
        let overrides = &stakes.overrides;
        let total_len = overrides.len() + stakes.stakes.len();

        let mut quotas = StakedStreamQuotas::with_capacity(total_len);

        for (&address, &stake) in overrides.iter() {
            quotas.insert(address, QuotaEntry::new(address, stake));
        }
        for (&address, &stake) in stakes.stakes.iter() {
            if !overrides.contains_key(&address) {
                quotas.insert(address, QuotaEntry::new(address, stake));
            }
        }
        quotas
    }

    fn with_capacity(capacity: usize) -> Self {
        StakedStreamQuotas {
            entries: HashMap::with_capacity(capacity),
            total_effective_stake: 1, //  to avoid zero division checks everywhere
        }
    }

    fn insert(&mut self, address: Pubkey, entry: QuotaEntry) {
        self.total_effective_stake += entry.effective_stake();
        self.entries.insert(address, Arc::new(entry));
    }
}

#[allow(clippy::arithmetic_side_effects)]
pub async fn refill_task(
    staked_stream_quotas_receiver: watch::Receiver<StakedStreamQuotas>,
    unstaked_connection_table: Arc<Mutex<ConnectionTable>>,
    max_tps: u64,
) {
    debug!("Spawning refill task with {max_tps} TPS");
    let token_fill_per_interval: u64 = max_tps * REFILL_INTERVAL.as_millis() as u64 / 1000;
    // initialize effective stake of unstaked connections based on their number
    let mut last_iter_unstaked_effective_stake = {
        let guard = unstaked_connection_table.lock().await;
        guard.total_size as u64 * BASE_STAKE_SOL
    };
    // initialize effective stake of staked connections as if everyone was connected
    let mut last_iter_staked_effective_stake = {
        let quotas = staked_stream_quotas_receiver.borrow();
        quotas.total_effective_stake
    };

    let mut interval = tokio::time::interval(REFILL_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
    loop {
        interval.tick().await; // first tick completes instantly
        let total_effective_stake;
        // fill the staked population's token buckets
        {
            let quotas = staked_stream_quotas_receiver.borrow();
            total_effective_stake =
                (last_iter_unstaked_effective_stake + last_iter_staked_effective_stake) + 1;
            last_iter_staked_effective_stake = 0;
            for entry in quotas.entries.values() {
                // fraction of total amount to deposit in this token bucket
                let my_fraction =
                    token_fill_per_interval * entry.effective_stake() / total_effective_stake;
                debug!(
                    "Grant {my_fraction} TXs to {} based on {}/{total_effective_stake} sol of \
                     stake.",
                    entry.address,
                    entry.effective_stake()
                );
                // maximal amount this bucket should be able to hold
                let my_max_tokens = token_fill_per_interval * MAX_BURST * entry.true_stake_sol
                    / total_effective_stake;
                // fill the bucket with all available tokens
                // recored effective stake of the entry after refill
                last_iter_staked_effective_stake += entry.refill(my_fraction, my_max_tokens);
            }
        }
        // fill the unstaked buckets
        // overflow does not cascade, as there is no fair order of redistribution
        // so we rely on proportional redistribution here
        {
            last_iter_unstaked_effective_stake = 0;
            let guard = unstaked_connection_table.lock().await;

            for (key, connection_entry_vec) in guard.iter() {
                if let Some(connection_entry) = connection_entry_vec.first() {
                    let entry: Arc<ConnectionStreamCounter> =
                        get_shared_state(connection_entry.stream_counter.clone())
                            .expect("Can not fail to downcast here");
                    let entry = &entry.quota;
                    let my_fraction =
                        token_fill_per_interval * entry.effective_stake() / total_effective_stake;
                    debug!(
                        "Grant {my_fraction} TXs to {key:?} based on {}/{total_effective_stake} \
                         sol of stake.",
                        entry.effective_stake()
                    );
                    // maximal amount this bucket should be able to hold
                    let my_max_tokens = token_fill_per_interval * MAX_BURST * entry.true_stake_sol
                        / total_effective_stake;
                    // fill the bucket with all available tokens
                    // recored effective stake of the entry after refill
                    last_iter_unstaked_effective_stake += entry.refill(my_fraction, my_max_tokens);
                }
            }
        }
    }
}

#[cfg(test)]
pub mod test {}
