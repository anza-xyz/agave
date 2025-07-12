use {
    governor::{DefaultDirectRateLimiter, DefaultKeyedRateLimiter, Quota, RateLimiter},
    lazy_lru::LruCache,
    std::{
        borrow::Borrow,
        net::IpAddr,
        num::NonZeroU32,
        sync::{
            atomic::{AtomicU64, Ordering},
            RwLock,
        },
        time::Instant,
    },
};

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

/// Enforces a rate limit on the volume of requests
/// per unit time.
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
    /// On success, returns Ok(amount of tokens left in the bucket)
    /// On failure, returns Err(amount of tokens missing to fill request)
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

    /// Retrieves monotonic time since bucket
    /// creation
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
        //dbg!(elapsed, tokens_per_us, new_tokens, time_to_return);
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

/// Provides rate limiting for multiple source IP addresses
/// on demand. Uses LazyLru under the hood.
pub struct KeyedRateLimiter {
    data: RwLock<LruCache<IpAddr, TokenBucket>>,
    prototype_bucket: TokenBucket,
}

impl KeyedRateLimiter {
    pub fn new(capacity: usize, prototype_bucket: TokenBucket) -> Self {
        Self {
            data: RwLock::new(LruCache::new(capacity)),
            prototype_bucket,
        }
    }
    pub fn current_tokens(&self, key: impl Borrow<IpAddr>) -> Option<u64> {
        let readguard = self.data.read().unwrap();
        let bucket = readguard.get(key.borrow())?;
        Some(bucket.current_tokens())
    }

    pub fn consume_tokens(&self, key: impl Borrow<IpAddr>, request_size: u64) -> Result<u64, u64> {
        // try to take read locks only (works for keys we have in the LRU)
        {
            let readguard = self.data.read().unwrap();
            let bucket = readguard.get(key.borrow());
            if let Some(bucket) = bucket {
                return bucket.consume_tokens(request_size);
            }
        }
        // if the key is not in the LRU, we need to allocate a new bucket
        let bucket = self.prototype_bucket.clone();
        let res = bucket.consume_tokens(request_size);
        // place the new bucket into the LRU
        {
            let mut writeguard = self.data.write().unwrap();
            writeguard.put(*key.borrow(), bucket);
        }
        res
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
        let rl = KeyedRateLimiter::new(8, prototype_bucket);
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
    fn bench_token_bucket() {
        let run_duration = Duration::from_secs(5);
        let tb = TokenBucket::new(1, 60, 100.0);
        let limiter = KeyedRateLimiter::new(2048, tb);

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
