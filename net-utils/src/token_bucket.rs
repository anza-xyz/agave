//! This module contains [`TokenBucket`], which provides ability to limit
//! rate of certain events, while allowing bursts through.
//! [`KeyedRateLimiter`] allows to rate-limit multiple keyed items, such
//! as connections.
use {
    cfg_if::cfg_if,
    dashmap::{mapref::entry::Entry, DashMap},
    solana_svm_type_overrides::sync::atomic::{AtomicU64, AtomicUsize, Ordering},
    std::{borrow::Borrow, cmp::Reverse, hash::Hash, time::Instant},
};

/// Enforces a rate limit on the volume of requests per unit time.
///
/// Instances update the amount of tokens upon access, and thus does not need to
/// be constantly polled to refill. Uses atomics internally so should be
/// relatively cheap to access from many threads
pub struct TokenBucket {
    new_tokens_per_us: f64,
    max_tokens: u64,
    /// bucket creation
    base_time: Instant,
    tokens: AtomicU64,
    /// time of last update in us since base_time
    last_update: AtomicU64,
    /// time unused in last token creation round
    credit_time_us: AtomicU64,
}

#[cfg(feature = "shuttle-test")]
static TIME_US: AtomicU64 = AtomicU64::new(0); //used to override Instant::now()

// If changing this impl, make sure to run benches and ensure they do not panic.
// much of the testing is impossible outside of real multithreading in release mode.
impl TokenBucket {
    /// Allocate a new TokenBucket
    pub fn new(initial_tokens: u64, max_tokens: u64, new_tokens_per_second: f64) -> Self {
        assert!(
            new_tokens_per_second > 0.0,
            "Token bucket can not have zero influx rate"
        );
        assert!(
            initial_tokens <= max_tokens,
            "Can not have more initial tokens than max tokens"
        );
        let base_time = Instant::now();
        TokenBucket {
            // recompute into us to avoid FP division on every update
            new_tokens_per_us: new_tokens_per_second / 1e6,
            max_tokens,
            tokens: AtomicU64::new(initial_tokens),
            last_update: AtomicU64::new(0),
            base_time,
            credit_time_us: AtomicU64::new(0),
        }
    }

    /// Return current amount of tokens in the bucket.
    /// This may be somewhat inconsistent across threads
    /// due to Relaxed atomics.
    #[inline]
    pub fn current_tokens(&self) -> u64 {
        let now = self.time_us();
        self.update_state(now);
        self.tokens.load(Ordering::Relaxed)
    }

    /// Attempts to consume tokens from bucket.
    ///
    /// On success, returns Ok(amount of tokens left in the bucket).
    /// On failure, returns Err(amount of tokens missing to fill request).
    #[inline]
    pub fn consume_tokens(&self, request_size: u64) -> Result<u64, u64> {
        let now = self.time_us();
        self.update_state(now);
        match self.tokens.fetch_update(
            Ordering::AcqRel,  // winner publishes new amount
            Ordering::Acquire, // everyone observed correct number
            |tokens| {
                if tokens >= request_size {
                    Some(tokens.saturating_sub(request_size))
                } else {
                    None
                }
            },
        ) {
            Ok(prev) => Ok(prev.saturating_sub(request_size)),
            Err(prev) => Err(request_size.saturating_sub(prev)),
        }
    }

    /// Returns time in microseconds until `num_tokens` worth of new
    /// tokens can be consumed.
    ///
    /// Calculation is performed assuming no demand for smaller
    /// batches of tokens (actual time may be longer).
    /// Returns None if num_tokens > bucket capacity.
    #[inline]
    pub fn us_to_have_tokens(&self, num_tokens: u64) -> Option<u64> {
        if num_tokens > self.max_tokens {
            return None;
        }

        match num_tokens.checked_sub(self.current_tokens()) {
            Some(missing) => Some((missing as f64 / self.new_tokens_per_us) as u64),
            None => Some(0),
        }
    }

    /// Retrieves monotonic time since bucket creation.
    fn time_us(&self) -> u64 {
        cfg_if! {
            if #[cfg(feature="shuttle-test")] {
                TIME_US.load(Ordering::Relaxed)
            } else {
                let now = Instant::now();
                let elapsed = now.saturating_duration_since(self.base_time);
                elapsed.as_micros() as u64
            }
        }
    }

    /// Updates internal state of the bucket by
    /// depositing new tokens (if appropriate)
    fn update_state(&self, now: u64) {
        // fetch last update time
        let last = self.last_update.load(Ordering::SeqCst);

        // If time has not advanced, nothing to do.
        if now <= last {
            return;
        }

        // Try to claim the interval [last, now].
        // If we can not claim it, someone else will claim [last..some other time] when they
        // touch the bucket.
        // If we can claim interval [last, now], no other thread can credit tokens for it anymore.
        // If [last, now] is too short to mint any tokens, spare time will be preserved in credit_time_us.
        match self.last_update.compare_exchange(
            last,
            now,
            Ordering::AcqRel,  // winner publishes new timestamp
            Ordering::Acquire, // loser observes updates
        ) {
            Ok(_) => {
                // This thread won the race and is responsible for minting tokens
                let elapsed = now.saturating_sub(last);

                // also add leftovers from previous conversion attempts.
                // we do not care about who uses the spare_time_us, so relaxed is ok here.
                let elapsed =
                    elapsed.saturating_add(self.credit_time_us.swap(0, Ordering::Relaxed));

                let new_tokens_f64 = elapsed as f64 * self.new_tokens_per_us;

                // amount of full tokens to be minted
                let new_tokens = new_tokens_f64.floor() as u64;

                let time_to_return = if new_tokens >= 1 {
                    // Credit tokens, saturating at max_tokens
                    let _ = self.tokens.fetch_update(
                        Ordering::AcqRel,  // writer publishes new amount
                        Ordering::Acquire, //we fetch the correct amount
                        |tokens| Some(tokens.saturating_add(new_tokens).min(self.max_tokens)),
                    );
                    // Fractional remainder of elapsed time (not enough to mint a whole token)
                    // that will be credited to other minters
                    (new_tokens_f64.fract() / self.new_tokens_per_us) as u64
                } else {
                    // No whole tokens minted → return whole interval
                    elapsed
                };
                // Save unused elapsed time for other threads
                self.credit_time_us
                    .fetch_add(time_to_return, Ordering::Relaxed);
            }
            Err(_) => {
                // Another thread advanced last_update first → nothing we can do now.
            }
        }
    }
}

impl Clone for TokenBucket {
    /// Clones the TokenBucket with approximate state
    /// of the original. While this will never return an object in an
    /// invalid state, using this in a contended environment is not recommended.
    fn clone(&self) -> Self {
        Self {
            new_tokens_per_us: self.new_tokens_per_us,
            max_tokens: self.max_tokens,
            base_time: self.base_time,
            tokens: AtomicU64::new(self.tokens.load(Ordering::Relaxed)),
            last_update: AtomicU64::new(self.last_update.load(Ordering::Relaxed)),
            credit_time_us: AtomicU64::new(self.credit_time_us.load(Ordering::Relaxed)),
        }
    }
}

/// Provides rate limiting for multiple contexts at the same time
///
/// This can use e.g. IP address as a Key.
/// Internally this is a [DashMap] of [TokenBucket] instances
/// that are created on demand using a prototype [TokenBucket]
/// to copy initial state from.
/// Uses LazyLru logic under the hood to keep the amount of items
/// under control.
pub struct KeyedRateLimiter<K>
where
    K: Hash + Eq,
{
    data: DashMap<K, TokenBucket>,
    target_capacity: usize,
    prototype_bucket: TokenBucket,
    countdown_to_shrink: AtomicUsize,
    approx_len: AtomicUsize,
    shrink_interval: usize,
}

impl<K> KeyedRateLimiter<K>
where
    K: Hash + Eq,
{
    /// Creates a new KeyedRateLimiter with a specified target capacity and shard amount for the
    /// underlying DashMap. This uses a LazyLRU style eviction policy, so actual memory consumption
    /// will be 2 * target_capacity.
    ///
    /// shard_amount should be greater than 0 and be a power of two.
    /// If a shard_amount which is not a power of two is provided, the function will panic.
    #[allow(clippy::arithmetic_side_effects)]
    pub fn new(target_capacity: usize, prototype_bucket: TokenBucket, shard_amount: usize) -> Self {
        let shrink_interval = target_capacity / 4;
        Self {
            data: DashMap::with_capacity_and_shard_amount(target_capacity * 2, shard_amount),
            target_capacity,
            prototype_bucket,
            countdown_to_shrink: AtomicUsize::new(shrink_interval),
            approx_len: AtomicUsize::new(0),
            shrink_interval,
        }
    }

    /// Fetches amount of tokens available for key.
    ///
    /// Returns None if no bucket exists for the key provided
    #[inline]
    pub fn current_tokens(&self, key: impl Borrow<K>) -> Option<u64> {
        let bucket = self.data.get(key.borrow())?;
        Some(bucket.current_tokens())
    }

    /// Consumes request_size tokens from a bucket at given key.
    ///
    /// On success, returns Ok(amount of tokens left in the bucket)
    /// On failure, returns Err(amount of tokens missing to fill request)
    /// If no bucket exists at key, a new bucket will be allocated, and normal policy will be applied to it
    /// Outdated buckets may be evicted on an LRU basis.
    pub fn consume_tokens(&self, key: K, request_size: u64) -> Result<u64, u64> {
        let (entry_added, res) = {
            let bucket = self.data.entry(key);
            match bucket {
                Entry::Occupied(entry) => (false, entry.get().consume_tokens(request_size)),
                Entry::Vacant(entry) => {
                    // if the key is not in the LRU, we need to allocate a new bucket
                    let bucket = self.prototype_bucket.clone();
                    let res = bucket.consume_tokens(request_size);
                    entry.insert(bucket);
                    (true, res)
                }
            }
        };

        if entry_added {
            if let Ok(count) =
                self.countdown_to_shrink
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                        if v == 0 {
                            // reset the countup to starting position
                            // thus preventing other threads from racing for locks
                            None
                        } else {
                            Some(v.saturating_sub(1))
                        }
                    })
            {
                if count == 1 {
                    // the last "previous" value we will see before counter reaches zero
                    self.maybe_shrink();
                    self.countdown_to_shrink
                        .store(self.shrink_interval, Ordering::Relaxed);
                }
            } else {
                self.approx_len.fetch_add(1, Ordering::Relaxed);
            }
        }
        res
    }

    /// Returns approximate amount of entries in the datastructure.
    /// Should be within ~10% of the true amount.
    #[inline]
    pub fn len_approx(&self) -> usize {
        self.approx_len.load(Ordering::Relaxed)
    }

    // apply lazy-LRU eviction policy to each DashMap shard.
    // Allowing side-effects here since overflows here are not
    // actually possible
    #[allow(clippy::arithmetic_side_effects)]
    fn maybe_shrink(&self) {
        let mut actual_len = 0;
        let target_shard_size = self.target_capacity / self.data.shards().len();
        let mut entries = Vec::with_capacity(target_shard_size * 2);
        for shardlock in self.data.shards() {
            let mut shard = shardlock.write();

            if shard.len() <= target_shard_size * 3 / 2 {
                actual_len += shard.len();
                continue;
            }
            entries.clear();
            entries.extend(
                shard.drain().map(|(key, value)| {
                    (key, value.get().last_update.load(Ordering::SeqCst), value)
                }),
            );

            entries.select_nth_unstable_by_key(target_shard_size, |(_, last_update, _)| {
                Reverse(*last_update)
            });

            shard.extend(
                entries
                    .drain(..)
                    .take(target_shard_size)
                    .map(|(key, _last_update, value)| (key, value)),
            );
            debug_assert!(shard.len() <= target_shard_size);
            actual_len += shard.len();
        }
        self.approx_len.store(actual_len, Ordering::Relaxed);
    }

    /// Set the auto-shrink interval. Set to 0 to disable shrinking.
    /// During writes we want to check for length, but not too often
    /// to reduce probability of lock contention, so keeping this
    /// large is good for perf (at cost of memory use)
    pub fn set_shrink_interval(&mut self, interval: usize) {
        self.shrink_interval = interval;
    }

    /// Get the auto-shrink interval.
    pub fn shrink_interval(&self) -> usize {
        self.shrink_interval
    }
}

#[cfg(test)]
pub mod test {
    use {
        super::*,
        solana_svm_type_overrides::thread,
        std::{
            net::{IpAddr, Ipv4Addr},
            time::Duration,
        },
    };

    #[test]
    fn test_token_bucket_basics() {
        let tb = TokenBucket::new(100, 100, 1000.0);
        assert_eq!(tb.current_tokens(), 100);
        tb.consume_tokens(50).expect("Bucket is initially full");
        tb.consume_tokens(50)
            .expect("We should still have >50 tokens left");
        tb.consume_tokens(50)
            .expect_err("There should not be enough tokens now");
        thread::sleep(Duration::from_millis(50));
        assert!(
            tb.current_tokens() > 40,
            "We should be refilling at ~1 token per millisecond"
        );
        assert!(
            tb.current_tokens() < 70,
            "We should be refilling at ~1 token per millisecond"
        );
        tb.consume_tokens(40)
            .expect("Bucket should have enough for another request now");
        thread::sleep(Duration::from_millis(120));
        assert_eq!(tb.current_tokens(), 100, "Bucket should not overfill");
    }

    #[test]
    fn test_token_bucket_us_to_have_tokens() {
        let tb = TokenBucket::new(1000, 1000, 1000.0);
        assert_eq!(tb.current_tokens(), 1000);
        tb.consume_tokens(1000).expect("Bucket is initially full");
        assert!(
            tb.current_tokens() < 100,
            "Shoult not have many tokens left in bucket"
        );

        let t = tb
            .us_to_have_tokens(500)
            .expect("500 < bucket capacity (1000)")
            / 1000; // convert to ms
        assert!(t > 100, "time to fill should be ~ 500ms (got {t})");
        assert!(t <= 500, "time to fill should be less than 500ms (got {t})");
    }

    #[test]
    fn test_keyed_rate_limiter() {
        let prototype_bucket = TokenBucket::new(100, 100, 1000.0);
        let rl = KeyedRateLimiter::new(8, prototype_bucket, 2);
        let ip1 = IpAddr::V4(Ipv4Addr::from_bits(1234));
        let ip2 = IpAddr::V4(Ipv4Addr::from_bits(4321));
        assert_eq!(rl.current_tokens(ip1), None, "Initially no buckets exist");
        rl.consume_tokens(ip1, 50)
            .expect("Bucket is initially full");
        rl.consume_tokens(ip1, 50)
            .expect("We should still have >50 tokens left");
        rl.consume_tokens(ip1, 50)
            .expect_err("There should not be enough tokens now");
        rl.consume_tokens(ip2, 50)
            .expect("Bucket is initially full");
        rl.consume_tokens(ip2, 50)
            .expect("We should still have >50 tokens left");
        rl.consume_tokens(ip2, 50)
            .expect_err("There should not be enough tokens now");
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            rl.current_tokens(ip1).unwrap() > 40,
            "We should be refilling at ~1 token per millisecond"
        );
        assert!(
            rl.current_tokens(ip1).unwrap() < 70,
            "We should be refilling at ~1 token per millisecond"
        );
        rl.consume_tokens(ip1, 40)
            .expect("Bucket should have enough for another request now");
        thread::sleep(Duration::from_millis(120));
        assert_eq!(
            rl.current_tokens(ip1),
            Some(100),
            "Bucket should not overfill"
        );
        assert_eq!(
            rl.current_tokens(ip2),
            Some(100),
            "Bucket should not overfill"
        );

        rl.consume_tokens(ip2, 100).expect("Bucket should be full");
        // go several times over the capacity of the TB to make sure old record
        // is erased no matter in which bucket it lands
        for ip in 0..64 {
            let ip = IpAddr::V4(Ipv4Addr::from_bits(ip));
            rl.consume_tokens(ip, 50).unwrap();
        }
        assert_eq!(
            rl.current_tokens(ip1),
            None,
            "Very old record should have been erased"
        );
        rl.consume_tokens(ip2, 100)
            .expect("New bucket should have been made for ip2");
    }

    #[cfg(feature = "shuttle-test")]
    #[test]
    fn shuttle_test_token_bucket_race() {
        use shuttle::sync::atomic::AtomicBool;
        shuttle::check_random(
            || {
                TIME_US.store(0, Ordering::SeqCst);
                let test_duration_us = 2500;
                let run: &AtomicBool = Box::leak(Box::new(AtomicBool::new(true)));
                let tb: &TokenBucket = Box::leak(Box::new(TokenBucket::new(10, 20, 5000.0)));

                // time advancement thread
                let time_advancer = thread::spawn(move || {
                    let mut current_time = 0;
                    while current_time < test_duration_us && run.load(Ordering::SeqCst) {
                        let increment = 100; // microseconds
                        current_time += increment;
                        TIME_US.store(current_time, Ordering::SeqCst);
                        shuttle::thread::yield_now();
                    }
                    run.store(false, Ordering::SeqCst);
                });

                let threads: Vec<_> = (0..2)
                    .map(|_| {
                        thread::spawn(move || {
                            let mut total = 0;
                            while run.load(Ordering::SeqCst) {
                                if tb.consume_tokens(5).is_ok() {
                                    total += 1;
                                }
                                shuttle::thread::yield_now();
                            }
                            total
                        })
                    })
                    .collect();

                time_advancer.join().unwrap();
                let received = threads.into_iter().map(|t| t.join().unwrap()).sum();

                // Initial tokens: 10, refill rate: 5000 tokens/sec (5 tokens/ms)
                // In 2ms: 10 + (5 * 2) = 20 tokens total
                // Each consumption: 5 tokens → 4 total consumptions expected
                assert_eq!(4, received);
            },
            100,
        );
    }
}
