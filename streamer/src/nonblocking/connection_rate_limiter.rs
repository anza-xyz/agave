use {
    dashmap::{mapref::entry::Entry, DashMap},
    governor::{DefaultDirectRateLimiter, DefaultKeyedRateLimiter, Quota, RateLimiter},
    std::{
        borrow::Borrow,
        cmp::Reverse,
        hash::Hash,
        net::IpAddr,
        num::NonZeroU32,
        sync::atomic::{AtomicU64, AtomicUsize, Ordering},
        time::Instant,
    },
};

/// Limits the rate of connections per IP address.
/// This has known issues and should be avoided.
pub struct ConnectionRateLimiter {
    limiter: DefaultKeyedRateLimiter<IpAddr>,
}

impl ConnectionRateLimiter {
    /// Create a new rate limiter per IpAddr. The rate is specified as the count per minute to allow for
    /// less frequent connections.
    pub fn new(limit_per_minute: u64) -> Self {
        let quota =
            Quota::per_minute(NonZeroU32::new(u32::try_from(limit_per_minute).unwrap()).unwrap());
        Self {
            limiter: DefaultKeyedRateLimiter::keyed(quota),
        }
    }

    /// Check if the connection from the said `ip` is allowed.
    pub fn is_allowed(&self, ip: &IpAddr) -> bool {
        // Acquire a permit from the rate limiter for the given IP address
        if self.limiter.check_key(ip).is_ok() {
            debug!("Request from IP {ip:?} allowed");
            true // Request allowed
        } else {
            debug!("Request from IP {ip:?} blocked");
            false // Request blocked
        }
    }

    /// retain only keys whose rate-limiting start date is within the rate-limiting interval.
    /// Otherwise drop them as inactive
    pub fn retain_recent(&self) {
        self.limiter.retain_recent()
    }

    /// Returns the number of "live" keys in the rate limiter.
    pub fn len(&self) -> usize {
        self.limiter.len()
    }

    /// Returns `true` if the rate limiter has no keys in it.
    pub fn is_empty(&self) -> bool {
        self.limiter.is_empty()
    }
}

/// Connection rate limiter for enforcing connection rates from
/// all clients.
/// This has known issues and should be avoided.
pub struct TotalConnectionRateLimiter {
    limiter: DefaultDirectRateLimiter,
}

impl TotalConnectionRateLimiter {
    /// Create a new rate limiter. The rate is specified as the count per second.
    pub fn new(limit_per_second: u64) -> Self {
        let quota =
            Quota::per_second(NonZeroU32::new(u32::try_from(limit_per_second).unwrap()).unwrap());
        Self {
            limiter: RateLimiter::direct(quota),
        }
    }

    /// Check if a connection is allowed.
    pub fn is_allowed(&self) -> bool {
        if self.limiter.check().is_ok() {
            true // Request allowed
        } else {
            false // Request blocked
        }
    }
}

/// Enforces a rate limit on the volume of requests per unit time.
///
/// Instances update the amount of tokens upon access, and thus does not
/// need to be constantly polled to refill.
/// Uses atomics internally so should be relatively cheap to access
/// from many threads
pub struct TokenBucket {
    tokens_per_second: f64,
    max_tokens: u64,
    base_time: Instant,
    tokens: AtomicU64,
    last_update: AtomicU64,
    spare_time: AtomicU64,
}

impl Clone for TokenBucket {
    fn clone(&self) -> Self {
        Self {
            tokens_per_second: self.tokens_per_second,
            max_tokens: self.max_tokens,
            base_time: self.base_time,
            tokens: AtomicU64::new(self.tokens.load(Ordering::SeqCst)),
            last_update: AtomicU64::new(self.last_update.load(Ordering::SeqCst)),
            spare_time: AtomicU64::new(self.spare_time.load(Ordering::SeqCst)),
        }
    }
}

impl TokenBucket {
    /// Allocate a new TokenBucket
    pub fn new(initial_tokens: u64, max_tokens: u64, tokens_per_second: f64) -> Self {
        assert!(
            tokens_per_second > 0.0,
            "Token bucket can not have zero influx rate"
        );
        assert!(
            initial_tokens <= max_tokens,
            "Can not have more initial tokens than max tokens"
        );
        let base_time = Instant::now();
        TokenBucket {
            tokens_per_second,
            max_tokens,
            tokens: AtomicU64::new(initial_tokens),
            last_update: AtomicU64::new(0),
            base_time,
            spare_time: AtomicU64::new(0),
        }
    }

    pub fn current_tokens(&self) -> u64 {
        let now = self.time_us();
        self.update_state(now);
        self.tokens.load(Ordering::Relaxed)
    }

    /// Attempts to consume tokens from bucket.
    ///
    /// On success, returns Ok(amount of tokens left in the bucket).
    /// On failure, returns Err(amount of tokens missing to fill request).
    pub fn consume_tokens(&self, request_size: u64) -> Result<u64, u64> {
        let now = self.time_us();
        self.update_state(now);
        match self
            .tokens
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |tokens| {
                if tokens >= request_size {
                    Some(tokens - request_size)
                } else {
                    None
                }
            }) {
            Ok(prev) => Ok(prev - request_size),
            Err(prev) => Err(request_size - prev),
        }
    }

    /// Retrieves monotonic time since bucket creation.
    fn time_us(&self) -> u64 {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.base_time);
        elapsed.as_micros() as u64
    }

    /// Updates internal state of the bucket by
    /// depositing new tokens (if appropriate)
    fn update_state(&self, now: u64) {
        // update last_access fist to prevent other threads from adding tokens
        // concurrent updaters will get elapsed = 0 or close to it
        let last_access = self.last_update.swap(now, Ordering::Relaxed);
        if now <= last_access {
            // this happens if some other thread has beaten us to state update
            return;
        }
        // use all actual elapsed time + all leftovers from other conversion attempts
        let elapsed = now - last_access + self.spare_time.swap(0, Ordering::Relaxed);

        let tokens_per_us = self.tokens_per_second / 1e6;
        let new_tokens = elapsed as f64 * tokens_per_us;
        // how much of the elapsed time we can not convert into tokens
        let mut time_to_return = (new_tokens.fract() / tokens_per_us) as u64;
        // check if we can mint at least 1 new token
        let new_tokens = new_tokens.floor() as u64;
        if new_tokens >= 1 {
            // fill the bucket. Ignore results since it is always OK here.
            let _ = self
                .tokens
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |tokens| {
                    Some(tokens.saturating_add(new_tokens).min(self.max_tokens))
                });
        } else {
            // no conversion, return all elapsed time to the pool
            time_to_return = elapsed;
        }
        // return the unused time resource we have borrowed by pretending
        // last update happened earlier than it truly did
        self.spare_time.fetch_add(time_to_return, Ordering::Relaxed);
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
}

impl<K> KeyedRateLimiter<K>
where
    K: Hash + Eq,
{
    /// Creates a new KeyedRateLimiter with a specified taget capacity and shard amount for the
    /// underlying DashMap. This uses a LazyLRU style eviction policy, so actual memory consumption
    /// will be 2 * target_capacity.
    ///
    /// shard_amount should greater than 0 and be a power of two.
    /// If a shard_amount which is not a power of two is provided, the function will panic.
    pub fn new(target_capacity: usize, prototype_bucket: TokenBucket, shard_amount: usize) -> Self {
        Self {
            data: DashMap::with_capacity_and_shard_amount(target_capacity * 2, shard_amount),
            target_capacity,
            prototype_bucket,
            countdown_to_shrink: AtomicUsize::new(0),
            approx_len: AtomicUsize::new(0),
        }
    }

    /// Fetches amount of tokens available for key.
    ///
    /// Returns None if no bucket exists for the key provided
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
            // we want to check for length, but not too often
            // this should reduce probability of lock contention
            let step = self.target_capacity / 4;
            if self
                .countdown_to_shrink
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                    if v >= step {
                        Some(0)
                    } else {
                        Some(v + 1)
                    }
                })
                .unwrap_or_default()
                >= step
            {
                self.maybe_shrink();
            }
        }
        res
    }

    /// Returns approximate amount of entries in the datastructure.
    /// Should be within ~10% of the true amount.
    pub fn len_approx(&self) -> usize {
        self.approx_len.load(Ordering::Relaxed)
    }

    // apply lazy-LRU eviction policy to each DashMap shard.
    fn maybe_shrink(&self) {
        let mut actual_len = 0;
        let target_shard_size = self.target_capacity / self.data.shards().len();
        for shardlock in self.data.shards() {
            let mut shard = shardlock.write();

            if shard.len() < target_shard_size.saturating_mul(2) {
                actual_len += shard.len();
                continue;
            }

            let mut entries: Vec<_> = shard
                .drain()
                .map(|(key, value)| (key, value.get().last_update.load(Ordering::Relaxed), value))
                .collect();

            entries.select_nth_unstable_by_key(target_shard_size, |(_, last_update, _)| {
                Reverse(*last_update)
            });

            shard.extend(
                entries
                    .into_iter()
                    .take(target_shard_size)
                    .map(|(key, _last_update, value)| (key, value)),
            );
            debug_assert!(shard.len() <= target_shard_size);
            actual_len += shard.len();
        }
        self.approx_len.store(actual_len, Ordering::Relaxed);
    }
}

#[cfg(test)]
pub mod test {
    use {
        super::*,
        std::{
            net::Ipv4Addr,
            sync::{
                atomic::{AtomicUsize, Ordering},
                Arc,
            },
            time::Duration,
        },
    };

    #[tokio::test]
    async fn test_total_connection_rate_limiter() {
        let limiter = TotalConnectionRateLimiter::new(2);
        assert!(limiter.is_allowed());
        assert!(limiter.is_allowed());
        assert!(!limiter.is_allowed());
    }

    #[tokio::test]
    async fn test_connection_rate_limiter() {
        let limiter = ConnectionRateLimiter::new(4);
        let ip1 = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1));
        assert!(limiter.is_allowed(&ip1));
        assert!(limiter.is_allowed(&ip1));
        assert!(limiter.is_allowed(&ip1));
        assert!(limiter.is_allowed(&ip1));
        assert!(!limiter.is_allowed(&ip1));

        assert!(limiter.len() == 1);
        let ip2 = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2));
        assert!(limiter.is_allowed(&ip2));
        assert!(limiter.len() == 2);
        assert!(limiter.is_allowed(&ip2));
        assert!(limiter.is_allowed(&ip2));
        assert!(limiter.is_allowed(&ip2));
        assert!(!limiter.is_allowed(&ip2));
    }

    #[test]
    fn test_token_bucket() {
        let tb = TokenBucket::new(100, 100, 1000.0);
        assert_eq!(tb.current_tokens(), 100);
        tb.consume_tokens(50).expect("Bucket is initially full");
        tb.consume_tokens(50)
            .expect("We should still have >50 tokens left");
        tb.consume_tokens(50)
            .expect_err("There should not be enough tokens now");
        std::thread::sleep(Duration::from_millis(50));
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
        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(tb.current_tokens(), 100, "Bucket should not overfill");
    }
    #[test]
    fn test_token_buckets() {
        let prototype_bucket = TokenBucket::new(100, 100, 1000.0);
        let rl = KeyedRateLimiter::new(8, prototype_bucket, 8);
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
        std::thread::sleep(Duration::from_millis(120));
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
        for ip in 0..16 {
            let ip = IpAddr::V4(Ipv4Addr::from_bits(ip));
            rl.consume_tokens(ip, 50).unwrap();
        }
        assert_eq!(
            rl.current_tokens(ip1),
            None,
            "Record should have been erased"
        );
        rl.consume_tokens(ip2, 100)
            .expect("New bucket should have been made for ip2");
    }

    #[test]
    fn bench_token_bucket_eviction() {
        let run_duration = Duration::from_secs(5);
        let target_size = 128;
        let tb = TokenBucket::new(1, 60, 100.0);
        let limiter = KeyedRateLimiter::new(target_size, tb, 8);

        let accepted = AtomicUsize::new(0);
        let rejected = AtomicUsize::new(0);

        let start = Instant::now();
        let ip_pool = 2048;
        let workers = 8;

        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    for i in 1.. {
                        if Instant::now() > start + run_duration {
                            break;
                        }
                        let ip = IpAddr::V4(Ipv4Addr::from_bits(i % ip_pool as u32));
                        if limiter.consume_tokens(ip, 1).is_ok() {
                            accepted.fetch_add(1, Ordering::Relaxed);
                        } else {
                            rejected.fetch_add(1, Ordering::Relaxed);
                        }
                        if limiter.data.len() > target_size * 2 + 1 {
                            eprintln!(
                                "Rate limiter grown over allowed size to {}!",
                                limiter.data.len()
                            );
                            panic!(
                                "Rate limiter grown over allowed size to {}!",
                                limiter.data.len()
                            );
                        }
                    }
                });
            }
        });

        let acc = accepted.load(Ordering::Relaxed);
        let rej = rejected.load(Ordering::Relaxed);
        println!("Run complete over {:?} seconds", run_duration.as_secs());
        println!("processed {} requests", acc + rej);
        println!("Rejected: {rej}");
    }

    #[test]
    fn bench_token_bucket() {
        let run_duration = Duration::from_secs(5);
        let tb = TokenBucket::new(1, 60, 100.0);
        let limiter = KeyedRateLimiter::new(2048, tb, 8);

        let accepted = AtomicUsize::new(0);
        let rejected = AtomicUsize::new(0);

        let start = Instant::now();
        let ip_pool = 2048;
        let expected_total_accepts = (run_duration.as_secs() * 100 * ip_pool) as i64;
        let workers = 8;

        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    for i in 1.. {
                        if Instant::now() > start + run_duration {
                            break;
                        }
                        let ip = IpAddr::V4(Ipv4Addr::from_bits(i % ip_pool as u32));
                        if limiter.consume_tokens(ip, 1).is_ok() {
                            accepted.fetch_add(1, Ordering::Relaxed);
                        } else {
                            rejected.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }
        });

        let acc = accepted.load(Ordering::Relaxed);
        let rej = rejected.load(Ordering::Relaxed);
        println!("Run complete over {:?} seconds", run_duration.as_secs());
        println!("Accepted: {acc} (target {expected_total_accepts})");
        println!("Rejected: {rej}");
        assert!(((acc as i64) - expected_total_accepts).abs() < expected_total_accepts / 10);
    }

    #[test]
    #[ignore = "does not pass"]
    fn bench_governor() {
        let run_duration = Duration::from_secs(5);
        let limiter = Arc::new(ConnectionRateLimiter::new(60 * 100));

        let accepted = AtomicUsize::new(0);
        let rejected = AtomicUsize::new(0);

        let start = Instant::now();
        let ip_pool = 2048;
        let expected_total_accepts = (run_duration.as_secs() * 100 * ip_pool) as i64;
        let workers = 8;

        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    for i in 1.. {
                        if Instant::now() > start + run_duration {
                            break;
                        }
                        let ip = IpAddr::V4(Ipv4Addr::from_bits(i % ip_pool as u32));
                        if limiter.is_allowed(&ip) {
                            accepted.fetch_add(1, Ordering::Relaxed);
                        } else {
                            rejected.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }
        });

        let acc = accepted.load(Ordering::Relaxed);
        let rej = rejected.load(Ordering::Relaxed);
        println!("Run complete over {:?} seconds", run_duration.as_secs());
        println!("Accepted: {acc} (target {expected_total_accepts})");
        println!("Rejected: {rej}");
        assert!(((acc as i64) - expected_total_accepts).abs() < expected_total_accepts / 10);
    }
}
