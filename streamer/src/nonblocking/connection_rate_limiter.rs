use {
    governor::{DefaultDirectRateLimiter, DefaultKeyedRateLimiter, Quota, RateLimiter},
    lazy_lru::LruCache,
    std::{borrow::Borrow, net::IpAddr, num::NonZeroU32, sync::Mutex, time::Instant},
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
#[derive(Clone)]
struct TokenBucketState {
    tokens: u64,
    last_access: u64,
}

/// Enforces a rate limit on the number of requests
/// over a period of time.
pub struct TokenBucket {
    tokens_per_second: f64,
    max_tokens: u64,
    base_time: Instant,
    state: Mutex<TokenBucketState>,
}

impl Clone for TokenBucket {
    fn clone(&self) -> Self {
        Self {
            tokens_per_second: self.tokens_per_second,
            max_tokens: self.max_tokens,
            base_time: self.base_time,
            state: Mutex::new(self.state.lock().unwrap().clone()),
        }
    }
}

impl TokenBucket {
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
            state: Mutex::new(TokenBucketState {
                tokens: initial_tokens,
                last_access: 0,
            }),
            base_time,
        }
    }

    pub fn current_tokens(&self) -> u64 {
        let now = self.time_us();
        let mut state = self.state.lock().unwrap();
        self.update_state(now, &mut state);
        state.tokens
    }

    pub fn consume_tokens(&self, request_size: u64) -> Result<u64, u64> {
        let now = self.time_us();
        let mut state = self.state.lock().unwrap();
        self.update_state(now, &mut state);
        if state.tokens >= request_size {
            state.tokens -= request_size;
            Ok(state.tokens)
        } else {
            Err(request_size - state.tokens)
        }
    }

    fn time_us(&self) -> u64 {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.base_time);
        elapsed.as_micros() as u64
    }

    fn update_state(&self, now: u64, state: &mut TokenBucketState) {
        debug_assert!(now >= state.last_access);
        let elapsed = (now - state.last_access) as f64;
        let new_tokens = elapsed * self.tokens_per_second / 1e6;
        // check if we can mint at least 1 new token
        if new_tokens > 1.0 {
            // update time of last mint
            state.last_access = now;
            // fill the bucket
            state.tokens = self
                .max_tokens
                .min(state.tokens.saturating_add(new_tokens as u64));
        }
    }
}

/// Provides rate limiting for multiple source IP addresses
/// on demand. Uses LazyLru under the hood.
pub struct KeyedRateLimiter {
    data: LruCache<IpAddr, TokenBucket>,
    prototype_bucket: TokenBucket,
}

impl KeyedRateLimiter {
    pub fn new(capacity: usize, prototype_bucket: TokenBucket) -> Self {
        Self {
            data: LruCache::new(capacity),
            prototype_bucket,
        }
    }
    pub fn current_tokens(&self, key: impl Borrow<IpAddr>) -> Option<u64> {
        let bucket = self.data.get(key.borrow())?;
        Some(bucket.current_tokens())
    }

    pub fn consume_tokens(
        &mut self,
        key: impl Borrow<IpAddr>,
        request_size: u64,
    ) -> Result<u64, u64> {
        let bucket = self.data.get(key.borrow());
        match bucket {
            Some(bucket) => bucket.consume_tokens(request_size),
            None => {
                let bucket = self.prototype_bucket.clone();
                let res = bucket.consume_tokens(request_size);
                self.data.put(*key.borrow(), bucket);
                res
            }
        }
    }
}

#[cfg(test)]
pub mod test {
    use {
        super::*,
        std::{net::Ipv4Addr, time::Duration},
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
        let mut rl = KeyedRateLimiter::new(8, prototype_bucket);
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
}
