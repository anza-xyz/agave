//! Allowlist policy: decides whether the local endpoint will accept
//! a TLS-attested peer pubkey.

use {
    arc_swap::ArcSwap,
    solana_pubkey::Pubkey,
    std::{
        collections::{HashMap, HashSet},
        net::IpAddr,
        sync::Arc,
    },
};

/// Called once per handshake, then periodically during connection lifetime.
///  A `false` answer closes the connection with the `NOT_ADMITTED` error code.
///
/// Implementations must be cheap - this is on the hot path of every new
/// connection.
pub trait Allowlist: Send + Sync + 'static {
    fn allow(&self, peer: &Pubkey) -> bool;

    /// Returns true if `ip` is a gossip-advertised address of an admitted
    /// peer. Consulted on the server-accept path *before* the TLS handshake
    /// (where the peer pubkey is not yet known): a validated source whose IP
    /// is not in this set is refused outright, without spending handshake CPU.
    /// This is the endpoint's airtight DoS gate, so the default is `false` -
    /// an impl that does not override it admits no inbound connections at all
    /// (loopback excepted, handled by the caller).
    fn allow_ip(&self, _ip: &IpAddr) -> bool {
        false
    }
}

/// Immutable generation of the allowed peer set: pubkeys with their epoch
/// stake, plus the gossip-advertised IPs those peers publish.
///
/// Stake values are available for future use (e.g. weighted eviction).
/// Peers inserted for test-only purposes (e.g. `extra_admit`) carry stake 0.
#[derive(Default)]
struct AllowSnapshot {
    peers: HashMap<Pubkey, u64>,
    ips: HashSet<IpAddr>,
}

/// Snapshot of the currently-allowed peer set. Producers (typically the
/// staked-validators cache, joined against gossip) call
/// [`StakedNodesAllowlist::swap`] to publish a new generation; consumers see
/// it on the next `allow` / `allow_ip` call.
#[derive(Default)]
pub struct StakedNodesAllowlist {
    inner: ArcSwap<AllowSnapshot>,
}

impl StakedNodesAllowlist {
    /// `peers` maps admitted pubkeys to epoch stake; `ips` is the set of
    /// gossip-advertised source IPs of those peers (the inbound admission gate,
    /// via [`Allowlist::allow_ip`]).
    pub fn new(peers: HashMap<Pubkey, u64>, ips: HashSet<IpAddr>) -> Self {
        Self {
            inner: ArcSwap::new(Arc::new(AllowSnapshot { peers, ips })),
        }
    }

    /// Publish a new allowlist generation. Atomic; no readers block.
    pub fn swap(&self, peers: HashMap<Pubkey, u64>, ips: HashSet<IpAddr>) {
        self.inner.store(Arc::new(AllowSnapshot { peers, ips }));
    }

    /// Number of allowed peers in the current generation.
    pub fn len(&self) -> usize {
        self.inner.load().peers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.load().peers.is_empty()
    }
}

impl Allowlist for StakedNodesAllowlist {
    fn allow(&self, peer: &Pubkey) -> bool {
        self.inner.load().peers.contains_key(peer)
    }

    fn allow_ip(&self, ip: &IpAddr) -> bool {
        self.inner.load().ips.contains(ip)
    }
}

/// Allow every peer. **Test/bench use only** - gated behind
/// `dev-context-only-utils` so production binaries cannot accidentally use it.
#[cfg(any(test, feature = "dev-context-only-utils"))]
pub struct AllowAll;

#[cfg(any(test, feature = "dev-context-only-utils"))]
impl Allowlist for AllowAll {
    fn allow(&self, _: &Pubkey) -> bool {
        true
    }

    fn allow_ip(&self, _: &IpAddr) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        std::net::{Ipv4Addr, Ipv6Addr},
    };

    #[test]
    fn allow_ip_matches_only_published_addrs() {
        let peer = Pubkey::new_unique();
        let known = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7));
        let allowlist = StakedNodesAllowlist::new(
            std::iter::once((peer, 100u64)).collect(),
            std::iter::once(known).collect(),
        );

        assert!(allowlist.allow(&peer));
        assert!(allowlist.allow_ip(&known), "published IP must be admitted");
        // Same /24, different host: NOT admitted (full-IP match, not subnet).
        assert!(!allowlist.allow_ip(&IpAddr::V4(Ipv4Addr::new(203, 0, 113, 8))));
        assert!(!allowlist.allow_ip(&IpAddr::V6(Ipv6Addr::LOCALHOST)));

        // A fresh generation with no IPs revokes admission.
        allowlist.swap(std::iter::once((peer, 100u64)).collect(), HashSet::new());
        assert!(allowlist.allow(&peer));
        assert!(!allowlist.allow_ip(&known));
    }

    #[test]
    fn default_allow_ip_is_false() {
        struct PubkeyOnly;
        impl Allowlist for PubkeyOnly {
            fn allow(&self, _: &Pubkey) -> bool {
                true
            }
        }
        assert!(!PubkeyOnly.allow_ip(&IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4))));
    }
}
