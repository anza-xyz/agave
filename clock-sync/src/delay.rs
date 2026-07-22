//! Per-peer one-way link delay calibration from the QUIC path RTT stamped
//! on each inbound datagram: `delay[j] = min RTT / 2`, with the minimum
//! taken over a sliding bucket window so a route change can expire it.

use {
    crate::clock::LocalNs,
    solana_pubkey::Pubkey,
    std::{collections::HashMap, time::Duration},
};

/// Sliding window: 10 buckets of 60 seconds each.
const BUCKET_WIDTH_NS: i64 = 60_000_000_000;
const NUM_BUCKETS: usize = 10;

#[derive(Debug, Clone, Copy)]
struct Bucket {
    /// Which absolute bucket (local_ns / BUCKET_WIDTH_NS) this slot holds.
    index: i64,
    min_rtt_ns: i64,
}

struct PeerDelay {
    /// Ring of per-bucket minima, keyed into by `bucket index % NUM_BUCKETS`.
    buckets: [Option<Bucket>; NUM_BUCKETS],
}

impl PeerDelay {
    fn new() -> Self {
        Self {
            buckets: [None; NUM_BUCKETS],
        }
    }

    fn observe(&mut self, rtt_ns: i64, now_ns: i64) {
        let index = now_ns.div_euclid(BUCKET_WIDTH_NS);
        let slot = &mut self.buckets[index.rem_euclid(NUM_BUCKETS as i64) as usize];
        match slot {
            Some(bucket) if bucket.index == index => {
                bucket.min_rtt_ns = bucket.min_rtt_ns.min(rtt_ns);
            }
            _ => {
                *slot = Some(Bucket {
                    index,
                    min_rtt_ns: rtt_ns,
                });
            }
        }
    }

    /// Minimum RTT over the buckets still inside the window.
    fn min_rtt_ns(&self, now_ns: i64) -> Option<i64> {
        let current = now_ns.div_euclid(BUCKET_WIDTH_NS);
        let oldest = current.saturating_sub(NUM_BUCKETS as i64).saturating_add(1);
        self.buckets
            .iter()
            .flatten()
            .filter(|bucket| bucket.index >= oldest)
            .map(|bucket| bucket.min_rtt_ns)
            .min()
    }
}

pub struct DelayTracker {
    peers: HashMap<Pubkey, PeerDelay>,
}

impl DelayTracker {
    pub fn new() -> Self {
        Self {
            peers: HashMap::new(),
        }
    }

    /// Record an RTT sample for `peer` taken at local time `now`.
    pub fn observe(&mut self, peer: Pubkey, rtt: Duration, now: LocalNs) {
        self.peers
            .entry(peer)
            .or_insert_with(PeerDelay::new)
            .observe(rtt.as_nanos() as i64, now.ns());
    }

    /// The calibrated one-way delay to `peer`: windowed min RTT / 2.
    pub fn delay_ns(&self, peer: &Pubkey, now: LocalNs) -> Option<i64> {
        Some(self.peers.get(peer)?.min_rtt_ns(now.ns())? / 2)
    }

    /// Drop peers no longer in the validator set.
    pub fn retain(&mut self, keep: impl Fn(&Pubkey) -> bool) {
        self.peers.retain(|peer, _| keep(peer));
    }

    /// (min, median, max) one-way delay across peers, for metrics.
    pub fn stats_ns(&self, now: LocalNs) -> Option<(i64, i64, i64)> {
        let mut delays: Vec<i64> = self
            .peers
            .values()
            .filter_map(|peer| Some(peer.min_rtt_ns(now.ns())? / 2))
            .collect();
        if delays.is_empty() {
            return None;
        }
        delays.sort_unstable();
        Some((
            delays[0],
            delays[delays.len() / 2],
            delays[delays.len().saturating_sub(1)],
        ))
    }
}

impl Default for DelayTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;

    const MS: i64 = 1_000_000;

    fn at(ns: i64) -> LocalNs {
        LocalNs::from_ns(ns)
    }

    #[test]
    fn tracks_windowed_min() {
        let mut tracker = DelayTracker::new();
        let peer = Pubkey::new_unique();
        let start = 1_000 * BUCKET_WIDTH_NS;
        tracker.observe(peer, Duration::from_millis(80), at(start));
        tracker.observe(peer, Duration::from_millis(50), at(start + BUCKET_WIDTH_NS));
        tracker.observe(
            peer,
            Duration::from_millis(90),
            at(start + 2 * BUCKET_WIDTH_NS),
        );
        // Min RTT 50ms -> one-way 25ms.
        assert_eq!(
            tracker.delay_ns(&peer, at(start + 2 * BUCKET_WIDTH_NS)),
            Some(25 * MS)
        );
    }

    #[test]
    fn old_minimum_expires_from_window() {
        let mut tracker = DelayTracker::new();
        let peer = Pubkey::new_unique();
        let start = 1_000 * BUCKET_WIDTH_NS;
        // A short route that later lengthens (e.g. a route change).
        tracker.observe(peer, Duration::from_millis(40), at(start));
        for i in 1..=(NUM_BUCKETS as i64) {
            tracker.observe(
                peer,
                Duration::from_millis(100),
                at(start + i * BUCKET_WIDTH_NS),
            );
        }
        // The 40ms sample fell out of the window; min is now 100ms.
        assert_eq!(
            tracker.delay_ns(&peer, at(start + (NUM_BUCKETS as i64) * BUCKET_WIDTH_NS)),
            Some(50 * MS)
        );
    }

    #[test]
    fn unknown_peer_has_no_delay() {
        let tracker = DelayTracker::new();
        assert_eq!(tracker.delay_ns(&Pubkey::new_unique(), at(0)), None);
    }

    #[test]
    fn stats_are_ordered() {
        let mut tracker = DelayTracker::new();
        let now = 1_000 * BUCKET_WIDTH_NS;
        for rtt_ms in [10, 30, 20] {
            tracker.observe(Pubkey::new_unique(), Duration::from_millis(rtt_ms), at(now));
        }
        assert_eq!(tracker.stats_ns(at(now)), Some((5 * MS, 10 * MS, 15 * MS)));
    }
}
