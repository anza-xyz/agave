use std::net::{IpAddr, SocketAddr};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SocketAddrSpace {
    Unspecified,
    Global,
}

impl SocketAddrSpace {
    pub fn new(allow_private_addr: bool) -> Self {
        if allow_private_addr {
            SocketAddrSpace::Unspecified
        } else {
            SocketAddrSpace::Global
        }
    }

    /// Returns true if the IP address is valid.
    #[inline]
    #[must_use]
    pub fn check(&self, addr: &SocketAddr) -> bool {
        // IPv4-only policy gate.
        let IpAddr::V4(addr) = addr.ip() else {
            return false;
        };

        if matches!(self, SocketAddrSpace::Unspecified) {
            return true;
        }

        // TODO: remove these once IpAddr::is_global is stable.
        // TODO: Consider excluding:
        // addr.is_link_local() || addr.is_broadcast()
        // || addr.is_documentation() || addr.is_unspecified()
        !(addr.is_private() || addr.is_loopback())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_global_rejects_ipv6_sockets() {
        let socket_addr_space = SocketAddrSpace::Global;
        let ipv6_addr: SocketAddr = "[2001:db8::1]:1234".parse().unwrap();
        let ipv4_mapped_ipv6_addr: SocketAddr = "[::ffff:8.8.8.8]:1234".parse().unwrap();

        assert!(!socket_addr_space.check(&ipv6_addr));
        assert!(!socket_addr_space.check(&ipv4_mapped_ipv6_addr));
    }

    #[test]
    fn test_unspecified_rejects_ipv6_sockets() {
        let socket_addr_space = SocketAddrSpace::Unspecified;
        let ipv6_addr: SocketAddr = "[2001:db8::1]:1234".parse().unwrap();
        let ipv4_mapped_ipv6_addr: SocketAddr = "[::ffff:8.8.8.8]:1234".parse().unwrap();

        assert!(!socket_addr_space.check(&ipv6_addr));
        assert!(!socket_addr_space.check(&ipv4_mapped_ipv6_addr));
    }
}
