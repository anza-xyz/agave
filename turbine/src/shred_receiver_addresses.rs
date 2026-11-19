//! Extra destinations for this validator's own leader shreds
//! (`--shred-receiver-address`).
//!
//! The list is validated once, here, so the broadcast path can trust it:
//! every address is IPv4 (the XDP sender routes IPv4 only), every port is
//! nonzero, and there are no duplicates. There is no maximum count; each
//! address costs one extra copy of the leader's shred stream.

use std::{collections::HashSet, net::SocketAddr, ops::Deref};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ShredReceiverAddressError {
    #[error("IPv6 shred receiver addresses are not supported: {0}")]
    Ipv6(SocketAddr),
    #[error("shred receiver address needs a nonzero port: {0}")]
    ZeroPort(SocketAddr),
}

/// Validated, deduplicated IPv4 destinations in configured order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShredReceiverAddresses(Vec<SocketAddr>);

impl ShredReceiverAddresses {
    /// Duplicates are dropped keeping the first occurrence, so the
    /// configured order is preserved.
    pub fn new(
        addrs: impl IntoIterator<Item = SocketAddr>,
    ) -> Result<Self, ShredReceiverAddressError> {
        let mut seen = HashSet::new();
        let mut unique = Vec::new();
        for addr in addrs {
            if addr.is_ipv6() {
                return Err(ShredReceiverAddressError::Ipv6(addr));
            }
            if addr.port() == 0 {
                return Err(ShredReceiverAddressError::ZeroPort(addr));
            }
            if seen.insert(addr) {
                unique.push(addr);
            }
        }
        Ok(Self(unique))
    }
}

// Makes ShredReceiverAddresses behave like &[SocketAddr].
impl Deref for ShredReceiverAddresses {
    type Target = [SocketAddr];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        std::net::{IpAddr, Ipv4Addr},
    };

    fn v4(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    }

    #[test]
    fn test_ipv6_rejected() {
        let addr: SocketAddr = "[::1]:9001".parse().unwrap();
        assert_eq!(
            ShredReceiverAddresses::new([v4(9001), addr]),
            Err(ShredReceiverAddressError::Ipv6(addr))
        );
    }

    #[test]
    fn test_port_zero_rejected() {
        assert_eq!(
            ShredReceiverAddresses::new([v4(0)]),
            Err(ShredReceiverAddressError::ZeroPort(v4(0)))
        );
    }

    #[test]
    fn test_dedup_keeps_first_occurrence_and_order() {
        let addrs = ShredReceiverAddresses::new([v4(9002), v4(9001), v4(9002), v4(9001)]).unwrap();
        assert_eq!(&addrs[..], &[v4(9002), v4(9001)]);
        assert_eq!(addrs.len(), 2);
        assert_eq!(addrs.iter().count(), 2);
    }

    #[test]
    fn test_empty_equals_default() {
        let addrs = ShredReceiverAddresses::new([]).unwrap();
        assert!(addrs.is_empty());
        assert_eq!(addrs, ShredReceiverAddresses::default());
    }

    #[test]
    fn test_no_cap() {
        let addrs = ShredReceiverAddresses::new((1..=1000).map(v4)).unwrap();
        assert_eq!(addrs.len(), 1000);
    }
}
