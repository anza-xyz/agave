//! Extra destinations for this validator's own leader shreds
//! (`--shred-receiver-address`).
//!
//! The list is validated once, here, so the broadcast path can trust it:
//! every address is IPv4 (turbine sends over IPv4), every
//! port is nonzero, there are no duplicates, and there are at most
//! [`MAX_SHRED_RECEIVER_ADDRESSES`]. Each address costs one extra copy of the
//! leader's shred stream, in the transmit queue the turbine packets use.

use std::{collections::HashSet, net::SocketAddr, ops::Deref};

/// The most receivers a leader sends copies to. During a burst of shreds the
/// copies sit in the transmit queue ahead of the next turbine packets, so the
/// count is bounded.
pub const MAX_SHRED_RECEIVER_ADDRESSES: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ShredReceiverAddressError {
    #[error("IPv6 shred receiver addresses are not supported: {0}")]
    Ipv6(SocketAddr),
    #[error("shred receiver address needs a nonzero port: {0}")]
    ZeroPort(SocketAddr),
    #[error(
        "too many shred receiver addresses: {0}, the maximum is {MAX_SHRED_RECEIVER_ADDRESSES}"
    )]
    TooMany(usize),
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
        if unique.len() > MAX_SHRED_RECEIVER_ADDRESSES {
            return Err(ShredReceiverAddressError::TooMany(unique.len()));
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
    fn test_cap_counts_distinct_addresses() {
        let max = MAX_SHRED_RECEIVER_ADDRESSES as u16;
        // Duplicates do not count toward the cap.
        let addrs = ShredReceiverAddresses::new((1..=max).chain(1..=max).map(v4)).unwrap();
        assert_eq!(addrs.len(), MAX_SHRED_RECEIVER_ADDRESSES);
        assert_eq!(
            ShredReceiverAddresses::new((1..=max + 1).map(v4)),
            Err(ShredReceiverAddressError::TooMany(
                MAX_SHRED_RECEIVER_ADDRESSES + 1
            ))
        );
    }
}
