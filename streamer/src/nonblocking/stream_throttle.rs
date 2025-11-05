use {
    crate::nonblocking::qos::ConnectionTableSharedState,
    crate::{nonblocking::quic::ConnectionPeerType, quic::StreamerStats, streamer::StakedNodes},
    percentage::Percentage,
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
    tokio::sync::watch,
    tokio::time::sleep,
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
pub struct StreamQuotas {
    pub mapping: HashMap<Pubkey, usize>,
    pub entries: Vec<Arc<QuotaEntry>>,
    pub total_stake: u64,
}

#[derive(Debug)]
pub struct QuotaEntry {
    pub stake: u64,
    pub number_of_times_throttled: AtomicU64,
    pub address: Pubkey,
    pub tokens: AtomicU64,
}

pub struct ConnectionStreamCounter {}

impl ConnectionStreamCounter {
    pub fn new() -> Self {
        Self {}
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

impl StreamQuotas {
    pub fn new(stakes: &StakedNodes) -> Self {
        let overrides = &stakes.overrides;
        let total_len = overrides.len() + stakes.stakes.len();
        let mut mapping = HashMap::with_capacity(total_len);
        let mut entries = Vec::with_capacity(total_len);
        let mut total_stake = 0;
        for (&address, &stake) in overrides.iter() {
            entries.push(Arc::new(QuotaEntry::new(address, stake)));
            total_stake += stake;
        }
        for (&address, &stake) in stakes.stakes.iter() {
            if !overrides.contains_key(&address) {
                entries.push(Arc::new(QuotaEntry::new(address, stake)));
                total_stake += stake;
            }
        }
        //entries.sort();
        for (index, entry) in entries.iter().enumerate() {
            mapping.insert(entry.address, index);
        }
        StreamQuotas {
            entries,
            mapping,
            total_stake,
        }
    }
}

#[allow(clippy::arithmetic_side_effects)]
pub async fn refill_task(test_stream_quotas_receiver: watch::Receiver<StreamQuotas>, max_tps: u64) {
    // amount of refill per interval
    let token_fill_rate: u64 = max_tps * 1000 / REFILL_INTERVAL.as_millis() as u64;
    loop {
        {
            let quotas = test_stream_quotas_receiver.borrow();
            let mut overflow = 0;
            let total_tokens_to_refill = (overflow + token_fill_rate).min(token_fill_rate * 2);

            for entry in quotas.entries.iter() {
                // fraction of total amount to deposit in this token bucket
                let my_fraction =
                    total_tokens_to_refill * entry.effective_stake() / quotas.total_stake;
                // maximal amount this bucket should be able to hold
                let my_max_tokens =
                    token_fill_rate * MAX_BURST * entry.effective_stake() / quotas.total_stake;

                // store any leftover tokens for next iteration of the fill loop
                overflow += entry.try_refill(my_fraction, my_max_tokens)
            }
        }
        tokio::time::sleep(REFILL_INTERVAL).await;
    }
}
// {
//         let base_fill_per_tick = 10000;
//         let max_tokens_in_bucket = 1000;
//         loop {
//             {
//                 let quotas = test_stream_quotas_receiver.borrow();

//                 let mut total_tokens_to_refill = 0;
//                 let mut overflow = 0;
//                 total_tokens_to_refill =
//                     (overflow + base_fill_per_tick).min(base_fill_per_tick * 2);
//                 dbg!(total_tokens_to_refill);

//                 for entry in quotas.entries.iter() {
//                     let my_fraction =
//                         total_tokens_to_refill * entry.stake / quotas.total_stake;
//                     let my_max_tokens =
//                         base_fill_per_tick * 2 * entry.stake / quotas.total_stake;

//                     dbg!(entry.address);
//                     // store any leftover tokens for next iteration of the fill loop
//                     overflow += entry.try_refill(my_fraction, my_max_tokens)
//                 }
//             }
//             tokio::time::sleep(REFILL_INTERVAL).await;
//         }
//     });
// }

#[cfg(test)]
pub mod test {}
