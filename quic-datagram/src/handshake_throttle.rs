//! Per-IP inbound-handshake throttle.
//!
//! The `allow_ip` admission gate already refuses sources that are not
//! gossip-advertised staked peers. This throttle closes the remaining hole: an
//! *allowlisted* peer (buggy, compromised, or malicious) that opens many TLS
//! handshakes at once. Each accepted handshake spawns a task that runs real
//! crypto and holds quinn state until it resolves, so unbounded concurrent
//! starts from one IP are a CPU/memory DoS.
//!
//! We bound a source IP to a single in-flight handshake by recording the last
//! *accepted* handshake start per IP and refusing any new one inside the
//! [`HANDSHAKE_COOLDOWN`] window. Because a pending handshake cannot outlive
//! `MAX_IDLE_TIMEOUT`, "one accept per window" implies "at most one in-flight
//! per IP" — without tracking completions.

use {
    crate::MAX_PEERS,
    lazy_lru::LruCache,
    std::{net::IpAddr, time::Instant},
};

/// Minimum spacing between accepted inbound handshakes from one IP. Equal to
/// the transport's `MAX_IDLE_TIMEOUT`: that is the longest a pending handshake
/// can live, so spacing accepts by it guarantees at most one in flight per IP.
const HANDSHAKE_COOLDOWN: std::time::Duration = crate::transport::MAX_IDLE_TIMEOUT;

/// Tracks the last accepted handshake start per source IP. Owned by the control
/// loop and touched from that single thread only, so no synchronization is
/// needed. LazyLRU caps memory at ~2× capacity; the live set is bounded by the
/// number of allowlisted IPs that handshake within a window (≤ [`MAX_PEERS`]).
pub(crate) struct HandshakeThrottle {
    recent: LruCache<IpAddr, Instant>,
}

impl HandshakeThrottle {
    pub fn new() -> Self {
        Self {
            recent: LruCache::new(MAX_PEERS as usize),
        }
    }

    /// Returns true if a fresh inbound handshake from `ip` may proceed.
    ///
    /// Loopback always admits (local-cluster / tests bind many endpoints on
    /// `127.0.0.1`, and local access is trusted). Otherwise admit iff `ip` has
    /// not had an accepted handshake within [`HANDSHAKE_COOLDOWN`]; on admit,
    /// record `now`. A refused attempt does **not** refresh the timestamp, so
    /// the window stays anchored to the last accepted start. `now` is a
    /// parameter for deterministic tests.
    pub fn admit(&mut self, ip: IpAddr, now: Instant) -> bool {
        if ip.is_loopback() {
            return true;
        }
        if let Some(&last) = self.recent.get(&ip)
            && now.duration_since(last) < HANDSHAKE_COOLDOWN
        {
            return false;
        }
        self.recent.put(ip, now);
        true
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        std::net::{Ipv4Addr, Ipv6Addr},
    };

    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    #[test]
    fn second_attempt_within_window_is_refused() {
        let mut t = HandshakeThrottle::new();
        let ip = v4(203, 0, 113, 7);
        let t0 = Instant::now();
        assert!(t.admit(ip, t0), "first attempt admitted");
        assert!(
            !t.admit(ip, t0 + HANDSHAKE_COOLDOWN / 2),
            "second attempt inside the window refused"
        );
        // Still refused just before the window closes.
        assert!(!t.admit(
            ip,
            t0 + HANDSHAKE_COOLDOWN - std::time::Duration::from_millis(1)
        ));
    }

    #[test]
    fn attempt_after_window_is_admitted() {
        let mut t = HandshakeThrottle::new();
        let ip = v4(203, 0, 113, 8);
        let t0 = Instant::now();
        assert!(t.admit(ip, t0));
        assert!(
            t.admit(ip, t0 + HANDSHAKE_COOLDOWN),
            "attempt at/after the window admitted"
        );
    }

    #[test]
    fn refused_attempt_does_not_extend_the_window() {
        let mut t = HandshakeThrottle::new();
        let ip = v4(203, 0, 113, 9);
        let t0 = Instant::now();
        assert!(t.admit(ip, t0));
        // A refused attempt mid-window must not push the window forward...
        assert!(!t.admit(ip, t0 + HANDSHAKE_COOLDOWN / 2));
        // ...so an attempt one full cooldown after the *accepted* start is let in.
        assert!(t.admit(ip, t0 + HANDSHAKE_COOLDOWN));
    }

    #[test]
    fn distinct_ips_are_independent() {
        let mut t = HandshakeThrottle::new();
        let t0 = Instant::now();
        assert!(t.admit(v4(1, 1, 1, 1), t0));
        assert!(t.admit(v4(2, 2, 2, 2), t0), "different IP unaffected");
    }

    #[test]
    fn loopback_is_never_throttled() {
        let mut t = HandshakeThrottle::new();
        let t0 = Instant::now();
        for i in 0..100 {
            assert!(t.admit(
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                t0 + std::time::Duration::from_millis(i)
            ));
            assert!(t.admit(
                IpAddr::V6(Ipv6Addr::LOCALHOST),
                t0 + std::time::Duration::from_millis(i)
            ));
        }
    }
}
