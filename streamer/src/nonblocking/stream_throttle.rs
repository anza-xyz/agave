//! This module implements the stake-weighted read throttling logic:
//! * Each connected client is assigned a fraction of total TPS budget R
//! in proportion to its stake.
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

/// Interval of refills for the QoS token buckets
pub const REFILL_INTERVAL: Duration = Duration::from_millis(100);

/// How many intervals worth of token refill rate do we accumulate
/// for idle connections (to handle bursts of arrivals)
const MAX_BURST: u64 = 2;

#[derive(Clone)]
pub struct StakedStreamQuotas {
    pub entries: HashMap<Pubkey, Arc<QuotaEntry>>,
    refill_order: Vec<Arc<QuotaEntry>>,
    pub total_effective_stake: u64,
}

#[derive(Debug)]
pub struct QuotaEntry {
    pub stake_sol: u64,
    pub number_of_times_throttled: AtomicU64,
    pub address: Pubkey,
    pub tokens: AtomicU64,
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
impl Ord for QuotaEntry {
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        // high stake comes first
        other.stake_sol.cmp(&self.stake_sol)
    }
}
impl PartialOrd for QuotaEntry {
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for QuotaEntry {
    fn eq(&self, other: &Self) -> bool {
        self.stake_sol == other.stake_sol
    }
}
impl Eq for QuotaEntry {}

impl QuotaEntry {
    pub fn new(address: Pubkey, stake_lamports: u64) -> Self {
        Self {
            stake_sol: stake_lamports / LAMPORTS_PER_SOL,
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
            if self.stake_sol == 0 {
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

    pub fn try_refill(&self, refill_amount: u64, my_max_tokens: u64) -> u64 {
        let current = self.tokens.load(Ordering::Relaxed);
        // this is technically a race, but since the other threads can only
        // decrement the token count, we are guaranteed to never allocate too many tokens
        let previous = self.tokens.fetch_max(
            my_max_tokens.min(current + refill_amount),
            Ordering::Relaxed,
        );

        let overflow = (previous + refill_amount).saturating_sub(my_max_tokens);
        overflow
    }

    fn effective_stake(&self) -> u64 {
        self.stake_sol + BASE_STAKE_SOL
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
        for entry in quotas.entries.values() {
            quotas.refill_order.push(entry.clone());
        }
        quotas.refill_order.sort();
        quotas
    }

    fn with_capacity(capacity: usize) -> Self {
        StakedStreamQuotas {
            entries: HashMap::with_capacity(capacity),
            refill_order: Vec::with_capacity(capacity),
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
    debug!("Spawning refill task");

    // amount of refill per interval
    let max_tps = max_tps.min(1000);
    let token_fill_rate: u64 = max_tps * 1000 / REFILL_INTERVAL.as_millis() as u64;
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

    let mut overflow = 0;
    loop {
        let total_tokens_to_refill = (overflow + token_fill_rate).min(token_fill_rate * 2);
        debug!("Total tokens to refill: {total_tokens_to_refill}, overflow {overflow}");
        overflow = 0;
        let total_effective_stake;
        // fill the staked population's token buckets
        {
            let quotas = staked_stream_quotas_receiver.borrow();
            // assume at least 1/32 of the effective stake is actually connected
            // avoid divisions by zero if no staked nodes are registered
            total_effective_stake = (last_iter_unstaked_effective_stake
                + last_iter_staked_effective_stake)
                .max(quotas.total_effective_stake / 32)
                + 1;
            last_iter_staked_effective_stake = 0;
            for entry in quotas.refill_order.iter() {
                // fraction of total amount to deposit in this token bucket
                let my_fraction =
                    total_tokens_to_refill * entry.effective_stake() / total_effective_stake;
                debug!(
                    "Grant {my_fraction} TXs to {} based on {}/{total_effective_stake} sol of stake.",
                    entry.address,
                    entry.effective_stake()
                );
                // maximal amount this bucket should be able to hold
                let my_max_tokens =
                    token_fill_rate * MAX_BURST * entry.effective_stake() / total_effective_stake;
                // fill the bucket with all available tokens
                let my_overflow = entry.try_refill(my_fraction, my_max_tokens);
                // record how well did we use our allocation
                last_iter_staked_effective_stake +=
                    entry.effective_stake() * my_fraction.saturating_sub(my_overflow) / my_fraction;
                // store any leftover tokens for next iteration of the fill loop
                overflow += my_overflow;
            }
        }
        // fill the unstaked buckets
        // overflow does not cascade, as there is no fair order of redistribution
        // so we rely on proportional redistribution here
        {
            last_iter_unstaked_effective_stake = 0;
            let guard = unstaked_connection_table.lock().await;

            for entry in guard.iter() {
                if let Some(entry) = entry.first() {
                    let entry: Arc<ConnectionStreamCounter> =
                        get_shared_state(entry.stream_counter.clone())
                            .expect("Can not fail to downcast here");
                    let entry = &entry.quota;
                    // max fraction of total amount to deposit in this token bucket
                    let my_fraction =
                        total_tokens_to_refill * entry.effective_stake() / total_effective_stake;
                    debug!(
                        "Grant {my_fraction} TXs to {} based on {}/{total_effective_stake} sol of stake.",
                        entry.address,
                        entry.effective_stake()
                    );
                    // maximal amount this bucket should be able to hold
                    let my_max_tokens = token_fill_rate * MAX_BURST * entry.effective_stake()
                        / total_effective_stake;

                    // fill the bucket with all available tokens
                    let my_overflow = entry.try_refill(my_fraction, my_max_tokens);
                    // record how well did we use our allocation
                    // if we used all our allocation, we add our effective stake to
                    // the consumption estimate for next round
                    // if everything went to overflow, we add no effective stake
                    last_iter_unstaked_effective_stake += entry.effective_stake()
                        * my_fraction.saturating_sub(my_overflow)
                        / my_fraction;
                    // store any leftover tokens for next iteration of the fill loop
                    overflow += my_overflow;
                }
            }
        }
        tokio::time::sleep(REFILL_INTERVAL).await;
    }
}

#[cfg(test)]
pub mod test {}
