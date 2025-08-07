use {
    governor::{DefaultDirectRateLimiter, DefaultKeyedRateLimiter, Quota, RateLimiter},
    std::{net::IpAddr, num::NonZeroU32},
};

/// Limits the rate of connections per IP address.
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
            time::{Duration, Instant},
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
    fn test_bench_rate_limiter() {
        let run_duration = Duration::from_secs(3);
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
    }
}
