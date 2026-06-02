//! Per-subnet connection-attempt rate limit on the server-accept path.
//!
//! Bounds peak CPU spent on TLS cert validation when an attacker spreads a
//! handshake flood across many source addresses. We gate **after** quinn's
//! stateless RETRY (so an off-path spoofer pays nothing here) and **before**
//! committing to the TLS handshake (so the cert + signature work is bounded
//! by the per-subnet burst budget).
//!
//! Same primitive (`KeyedRateLimiter` + `TokenBucket`) as
//! `streamer::nonblocking::connection_rate_limiter::ConnectionRateLimiter`,
//! but keyed on the IPv4 **/24 subnet prefix** rather than the full address.
//! /24 is the operational unit of an attacker pool - they share an upstream
//! allocation and rotating host bits inside a subnet shouldn't reset the
//! budget. IPv6 is rejected outright at `dispatch_accept` so this limiter
//! only ever sees v4 keys.
use {
    solana_net_utils::token_bucket::{KeyedRateLimiter, TokenBucket},
    std::net::{IpAddr, Ipv4Addr},
};

/// Maximum handshake attempts a single subnet can burn through before the
/// bucket is empty. Once exhausted, further attempts from that subnet are
/// refused until the refill timer credits a token.
pub const BURST_PER_SUBNET: u64 = 100;

/// Refill cadence in attempts per minute. Sustained legitimate use sits at
/// roughly one new staked-validator dial per minute per subnet, so this
/// rate matches expected workload while making a flood costly.
pub const REFILL_PER_MINUTE: u64 = 1;

/// Target entries retained. Each entry is roughly 120 B (subnet key,
/// TokenBucket state, DashMap overhead). Under the LazyLRU's 2× growth
/// ceiling, this caps memory near 100 MB.
const TARGET_CAPACITY: usize = 350_000;

/// DashMap shard count. Power of two; sized for typical async-reactor
/// contention on the accept path.
const SHARDS: usize = 64;

pub(crate) struct SubnetRateLimiter {
    inner: KeyedRateLimiter<IpAddr>,
}

impl SubnetRateLimiter {
    pub fn new() -> Self {
        let prototype = TokenBucket::new(
            BURST_PER_SUBNET,
            BURST_PER_SUBNET,
            REFILL_PER_MINUTE as f64 / 60.0,
        );
        Self {
            inner: KeyedRateLimiter::new(TARGET_CAPACITY, prototype, SHARDS),
        }
    }

    /// Returns true if a handshake from `ip` is admitted (a token was
    /// consumed from the subnet bucket); false if the subnet has exhausted
    /// its burst budget.
    ///
    /// Loopback addresses bypass the limit entirely - they are trusted
    /// implicitly.
    pub fn admit(&self, ip: IpAddr) -> bool {
        if ip.is_loopback() {
            return true;
        }
        self.inner.consume_tokens(subnet_key(ip), 1).is_ok()
    }
}

/// Normalise a source address to its subnet aggregate. IPv4 → /24.
fn subnet_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            IpAddr::V4(Ipv4Addr::new(o[0], o[1], o[2], 0))
        }
        // We do not support v6, but do not want to panic here either
        IpAddr::V6(_v6) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
    }
}

#[cfg(test)]
mod tests {
    use {super::*, std::net::Ipv4Addr};

    #[test]
    fn subnet_key_collapses_v4_host_bits() {
        let a = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        let b = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 254));
        assert_eq!(subnet_key(a), subnet_key(b));
        let c = IpAddr::V4(Ipv4Addr::new(10, 0, 1, 1));
        assert_ne!(subnet_key(a), subnet_key(c));
    }

    #[test]
    fn burst_then_exhausts() {
        let lim = SubnetRateLimiter::new();
        let ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7));
        for _ in 0..BURST_PER_SUBNET {
            assert!(lim.admit(ip), "should admit during burst");
        }
        assert!(!lim.admit(ip), "should refuse after burst exhausted");
    }

    #[test]
    fn host_bits_do_not_reset_budget() {
        let lim = SubnetRateLimiter::new();
        for host in 0..BURST_PER_SUBNET as u8 {
            let ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, host));
            assert!(lim.admit(ip));
        }
        let next = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 200));
        assert!(
            !lim.admit(next),
            "rotating host bits in the same /24 must not bypass the limit"
        );
    }

    #[test]
    fn loopback_bypasses_limit() {
        let lim = SubnetRateLimiter::new();
        // 5× burst from loopback - must all admit since the limit doesn't
        // apply to loopback (per project conduct rule: localhost is
        // trusted).
        for _ in 0..BURST_PER_SUBNET * 5 {
            assert!(lim.admit(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        }
    }
}
