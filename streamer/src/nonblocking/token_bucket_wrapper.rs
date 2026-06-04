use std::{
    sync::atomic::{AtomicI64, AtomicU64, Ordering},
    time::{Duration, Instant},
};

use solana_net_utils::token_bucket::TokenBucket;

/// Global token-bucket load estimator.
///
/// Connections consume tokens via [`acquire`]. The system is considered
/// saturated when the bucket level drops below `burst_capacity / 10`.
///
/// Refills are driven by [`acquire`]: when the level drops below half
/// capacity, a time-proportional refill is attempted, capped at
/// `burst_capacity`.
pub struct GlobalLoadTrackerTokenBucket {
    bucket: TokenBucket,
    burst_capacity: u64,
}

impl GlobalLoadTrackerTokenBucket {
    pub(crate) fn new(
        max_streams_per_second: u64,
        burst_capacity: u64,
        refill_interval: Duration,
    ) -> Self {
        Self {
            bucket: TokenBucket::new(
                burst_capacity,
                burst_capacity,
                max_streams_per_second,
                Duration::from_secs(1),
            ),
            burst_capacity,
        }
    }

    /// Consume one token. Triggers a refill attempt when the bucket
    /// drops below `burst_capacity / 10`.
    pub(crate) fn acquire(&self) {
        self.bucket.consume_tokens(1);
    }

    /// Return whether the system is saturated.
    ///
    /// The system is saturated when the bucket level is below
    /// `burst_capacity / 10`. When already below that threshold,
    /// a refill is attempted so parked connections can detect recovery
    /// even when no streams are flowing.
    pub fn is_saturated(&self) -> bool {
        self.bucket.current_tokens() < self.burst_capacity / 10
    }

    /// Return the current bucket level.
    pub fn bucket_level(&self) -> i64 {
        self.bucket.current_tokens() as i64
    }
}
