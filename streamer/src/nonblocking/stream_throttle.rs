use {
    crate::{
        nonblocking::{
            qos::{get_shared_state, ConnectionTableSharedState},
            quic::ConnectionTable,
        },
        quic::StreamerStats,
        streamer::StakedNodes,
    },
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
pub const BASE_STAKE: u64 = 1000;

/// Interval of refills for the QoS token buckets
pub const REFILL_INTERVAL: Duration = Duration::from_millis(10);

/// How many intervals worth of token refill rate do we accumulate
/// for idle connections (to handle bursts of arrivals)
const MAX_BURST: u64 = 2;

#[derive(Clone)]
pub struct StakedStreamQuotas {
    pub entries: HashMap<Pubkey, Arc<QuotaEntry>>,
    refill_order: Vec<Arc<QuotaEntry>>,
    pub total_stake: u64,
}

#[derive(Debug)]
pub struct QuotaEntry {
    pub stake: u64,
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
        other.stake.cmp(&self.stake)
    }
}
impl PartialOrd for QuotaEntry {
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for QuotaEntry {
    fn eq(&self, other: &Self) -> bool {
        self.stake == other.stake
    }
}
impl Eq for QuotaEntry {}

impl QuotaEntry {
    pub fn new(address: Pubkey, stake: u64) -> Self {
        Self {
            stake: stake,
            tokens: AtomicU64::new(128),
            address,
            number_of_times_throttled: AtomicU64::new(0),
        }
    }

    /// try to consume a token from the throttler, if it can not it will block.
    pub async fn wait_for_token(&self, stats: &StreamerStats) {
        // TODO optimize
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
            if self.stake == 0 {
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
        // TODO optimize
        let current = self.tokens.load(Ordering::Relaxed);
        // this is technically a race, but since the other threads can only
        // decrement the token count, we are guaranteed to never allocate too many tokens
        let previous = self.tokens.fetch_max(
            my_max_tokens.min(current + refill_amount),
            Ordering::Relaxed,
        );

        let overflow = (previous + refill_amount).saturating_sub(my_max_tokens);
        debug!(
            "Refilled {} for {refill_amount}, overflow {overflow}",
            self.address
        );
        overflow
    }

    fn effective_stake(&self) -> u64 {
        self.stake + BASE_STAKE
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

    pub fn with_capacity(capacity: usize) -> Self {
        StakedStreamQuotas {
            entries: HashMap::with_capacity(capacity),
            refill_order: Vec::with_capacity(capacity),
            total_stake: 1, //  to avoid zero division checks everywhere
        }
    }

    fn insert(&mut self, address: Pubkey, entry: QuotaEntry) {
        self.total_stake += entry.stake;
        self.entries.insert(address, Arc::new(entry));
    }
}

#[allow(clippy::arithmetic_side_effects)]
pub async fn refill_task(
    staked_stream_quotas_receiver: watch::Receiver<StakedStreamQuotas>,
    unstaked_connection_table: Arc<Mutex<ConnectionTable>>,
    max_tps: u64,
) {
    // amount of refill per interval
    let token_fill_rate: u64 = max_tps * 1000 / REFILL_INTERVAL.as_millis() as u64;
    let mut last_iter_unstaked_effective_stake = 0;
    let mut overflow = 0;
    loop {
        let total_tokens_to_refill = (overflow + token_fill_rate).min(token_fill_rate * 2);
        overflow = 0;
        let total_stake;
        // fill the staked portion
        // overflow from higher staked nodes immediately cascades to the lower staked nodes
        {
            let quotas = staked_stream_quotas_receiver.borrow();
            total_stake = last_iter_unstaked_effective_stake + quotas.total_stake;
            for entry in quotas.refill_order.iter() {
                // fraction of total amount to deposit in this token bucket
                let my_fraction = total_tokens_to_refill * entry.effective_stake() / total_stake;
                // maximal amount this bucket should be able to hold
                let my_max_tokens =
                    token_fill_rate * MAX_BURST * entry.effective_stake() / total_stake;

                // fill the bucket with all available tokens (new + overflow from last iter)
                // store any leftover tokens for next iteration of the fill loop
                overflow = entry.try_refill(my_fraction + overflow, my_max_tokens)
            }
        }
        // fill the unstaked buckets
        // overflow does not cascade, as there is no fair order of redistribution
        // so we rely on proportional redistribution here
        {
            let guard = unstaked_connection_table.lock().await;
            last_iter_unstaked_effective_stake = guard.total_size as u64 * BASE_STAKE;
            for entry in guard.iter() {
                if let Some(entry) = entry.first() {
                    let entry: Arc<ConnectionStreamCounter> =
                        get_shared_state(entry.stream_counter.clone())
                            .expect("Can not fail to downcast here");
                    let entry = &entry.quota;
                    // fraction of total amount to deposit in this token bucket
                    let my_fraction =
                        total_tokens_to_refill * entry.effective_stake() / total_stake;
                    // maximal amount this bucket should be able to hold
                    let my_max_tokens =
                        token_fill_rate * MAX_BURST * entry.effective_stake() / total_stake;

                    // store any leftover tokens for next iteration of the fill loop
                    overflow += entry.try_refill(my_fraction, my_max_tokens)
                }
            }
        }
        tokio::time::sleep(REFILL_INTERVAL).await;
    }
}

#[cfg(test)]
pub mod test {}
