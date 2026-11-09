//! Extra destinations for this validator's own leader shreds.
//!
//! `--shred-receiver-address` mirrors every shred the leader broadcasts to a
//! configured list of IPv4 socket addresses, after the turbine packets. The
//! list is parsed once at startup; this module holds the parser and its
//! limits. Compatible with the flag of the same name in Jito-Agave.

use std::net::{SocketAddr, ToSocketAddrs};

/// Upper bound on configured receiver addresses. Keeps the extra per-shred
/// send fan-out bounded.
pub const MAX_SHRED_RECEIVER_ADDRESSES: usize = 32;

pub type ShredReceiverAddresses = Vec<SocketAddr>;

fn push_unique_addr(
    addrs: &mut ShredReceiverAddresses,
    socket_addr: SocketAddr,
) -> Result<(), String> {
    // The XDP sender routes IPv4 only.
    if socket_addr.is_ipv6() {
        return Err(format!(
            "IPv6 shred receiver addresses are not supported: {socket_addr}"
        ));
    }
    if socket_addr.port() == 0 {
        return Err(format!(
            "shred receiver address needs a port: {socket_addr}"
        ));
    }
    if addrs.contains(&socket_addr) {
        return Ok(());
    }
    if addrs.len() >= MAX_SHRED_RECEIVER_ADDRESSES {
        return Err(format!(
            "too many shred receiver addresses: maximum is {MAX_SHRED_RECEIVER_ADDRESSES}"
        ));
    }
    addrs.push(socket_addr);
    Ok(())
}

/// Parses `--shred-receiver-address` values: each value holds one or more
/// comma-separated `ip:port` or `host:port` entries. A hostname is resolved
/// here, once, and every IPv4 address it resolves to becomes a destination;
/// IPv6 and port 0 are errors. Duplicates are dropped; more than
/// [`MAX_SHRED_RECEIVER_ADDRESSES`] unique addresses is an error.
pub fn parse_shred_receiver_addresses<I: IntoIterator<Item = S>, S: AsRef<str>>(
    values: I,
) -> Result<ShredReceiverAddresses, String> {
    let mut addrs = ShredReceiverAddresses::new();
    for value in values {
        for addr in value
            .as_ref()
            .split(',')
            .map(str::trim)
            .filter(|addr| !addr.is_empty())
        {
            // A literal socket address needs no DNS lookup.
            if let Ok(socket_addr) = addr.parse::<SocketAddr>() {
                push_unique_addr(&mut addrs, socket_addr)?;
                continue;
            }
            let resolved = addr
                .to_socket_addrs()
                .map_err(|err| format!("unable to resolve host {addr}: {err}"))?;
            let mut resolved_any_ipv4 = false;
            for socket_addr in resolved.filter(SocketAddr::is_ipv4) {
                resolved_any_ipv4 = true;
                push_unique_addr(&mut addrs, socket_addr)?;
            }
            if !resolved_any_ipv4 {
                return Err(format!(
                    "unable to resolve host to any IPv4 address: {addr}"
                ));
            }
        }
    }
    Ok(addrs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_empty() {
        let addrs = parse_shred_receiver_addresses([""]).unwrap();
        assert!(addrs.is_empty());
    }

    #[test]
    fn test_parse_multiple_empty_values_disable() {
        let addrs = parse_shred_receiver_addresses(["", "   "]).unwrap();
        assert!(addrs.is_empty());
    }

    #[test]
    fn test_parse_multiple_and_dedup() {
        let addrs =
            parse_shred_receiver_addresses(["127.0.0.1:9001,127.0.0.1:9002", "127.0.0.1:9001"])
                .unwrap();
        let expected = [
            "127.0.0.1:9001".parse().unwrap(),
            "127.0.0.1:9002".parse().unwrap(),
        ];
        assert_eq!(addrs.as_slice(), &expected);
    }

    #[test]
    fn test_parse_hostname_with_dns_resolution_ipv4_only() {
        let addrs = parse_shred_receiver_addresses(["localhost:9003"]).unwrap();
        assert!(!addrs.is_empty());
        assert!(addrs.iter().all(SocketAddr::is_ipv4));
        assert!(addrs.iter().all(|addr| addr.port() == 9003));
    }

    #[test]
    fn test_parse_explicit_ipv6_socket_addr_rejected() {
        let err = parse_shred_receiver_addresses(["[::1]:9003"]).unwrap_err();
        assert!(err.contains("IPv6 shred receiver addresses are not supported"));
    }

    #[test]
    fn test_parse_port_zero_rejected() {
        let err = parse_shred_receiver_addresses(["127.0.0.1:0"]).unwrap_err();
        assert!(err.contains("needs a port"));
    }

    #[test]
    fn test_parse_invalid() {
        assert!(parse_shred_receiver_addresses(["not-a-host"]).is_err());
    }

    #[test]
    fn test_parse_max_addresses() {
        let values = (0..=MAX_SHRED_RECEIVER_ADDRESSES)
            .map(|index| format!("127.0.0.1:{}", 10_000 + index))
            .collect::<Vec<_>>();
        let err = parse_shred_receiver_addresses(values.iter().map(String::as_str)).unwrap_err();
        assert!(err.contains(&MAX_SHRED_RECEIVER_ADDRESSES.to_string()));
    }
}
