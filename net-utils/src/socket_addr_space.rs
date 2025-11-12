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

    #[inline]
    #[must_use]
    pub fn check(&self, addr: &SocketAddr) -> bool {
        if matches!(self, SocketAddrSpace::Unspecified) {
            return true;
        }
        match addr.ip() {
            IpAddr::V4(addr) => {
                !(addr.is_private()
                    || addr.is_loopback()
                    || addr.is_link_local()
                    || addr.is_broadcast()
                    || addr.is_documentation()
                    || addr.is_unspecified()
                    || addr.is_multicast())
            }
            IpAddr::V6(addr) => {
                !(addr.is_loopback()
                    || addr.is_unspecified()
                    || addr.is_multicast()
                    || addr.is_unicast_link_local())
            }
        }
    }

    #[inline]
    #[must_use]
    pub fn is_multicast(&self, addr: &SocketAddr) -> bool {
        match addr.ip() {
            IpAddr::V4(addr) => addr.is_multicast(),
            IpAddr::V6(addr) => addr.is_multicast(),
        }
    }

    #[inline]
    #[must_use]
    pub fn is_private(&self, addr: &SocketAddr) -> bool {
        match addr.ip() {
            IpAddr::V4(addr) => addr.is_private(),
            IpAddr::V6(addr) => addr.is_unicast_link_local() || addr.is_unique_local(),
        }
    }

    #[inline]
    #[must_use]
    pub fn is_loopback(&self, addr: &SocketAddr) -> bool {
        addr.ip().is_loopback()
    }

    #[inline]
    #[must_use]
    pub fn is_link_local(&self, addr: &SocketAddr) -> bool {
        match addr.ip() {
            IpAddr::V4(addr) => addr.is_link_local(),
            IpAddr::V6(addr) => addr.is_unicast_link_local(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv6Addr};

    #[test]
    fn test_socket_addr_space_new() {
        assert_eq!(
            SocketAddrSpace::new(true),
            SocketAddrSpace::Unspecified
        );
        assert_eq!(SocketAddrSpace::new(false), SocketAddrSpace::Global);
    }

    #[test]
    fn test_check_unspecified() {
        let space = SocketAddrSpace::Unspecified;
        assert!(space.check(&SocketAddr::from(([127, 0, 0, 1], 8080))));
        assert!(space.check(&SocketAddr::from(([10, 0, 0, 1], 8080))));
        assert!(space.check(&SocketAddr::from(([224, 0, 0, 1], 8080))));
    }

    #[test]
    fn test_check_global() {
        let space = SocketAddrSpace::Global;
        assert!(space.check(&SocketAddr::from(([8, 8, 8, 8], 53))));
        assert!(!space.check(&SocketAddr::from(([10, 0, 0, 1], 8080))));
        assert!(!space.check(&SocketAddr::from(([192, 168, 1, 1], 8080))));
        assert!(!space.check(&SocketAddr::from(([127, 0, 0, 1], 8080))));
        assert!(!space.check(&SocketAddr::from(([224, 0, 0, 1], 8080))));
        assert!(!space.check(&SocketAddr::from(([169, 254, 0, 1], 8080))));
    }

    #[test]
    fn test_is_multicast() {
        let space = SocketAddrSpace::Global;
        assert!(space.is_multicast(&SocketAddr::from(([224, 0, 0, 1], 8080))));
        assert!(!space.is_multicast(&SocketAddr::from(([8, 8, 8, 8], 53))));
    }

    #[test]
    fn test_is_private() {
        let space = SocketAddrSpace::Global;
        assert!(space.is_private(&SocketAddr::from(([10, 0, 0, 1], 8080))));
        assert!(space.is_private(&SocketAddr::from(([192, 168, 1, 1], 8080))));
        assert!(!space.is_private(&SocketAddr::from(([8, 8, 8, 8], 53))));
    }

    #[test]
    fn test_is_loopback() {
        let space = SocketAddrSpace::Global;
        assert!(space.is_loopback(&SocketAddr::from(([127, 0, 0, 1], 8080))));
        assert!(!space.is_loopback(&SocketAddr::from(([8, 8, 8, 8], 53))));
    }

    #[test]
    fn test_is_link_local() {
        let space = SocketAddrSpace::Global;
        assert!(space.is_link_local(&SocketAddr::from(([169, 254, 0, 1], 8080))));
        assert!(!space.is_link_local(&SocketAddr::from(([8, 8, 8, 8], 53))));
    }

    #[test]
    fn test_ipv6_addresses() {
        let space = SocketAddrSpace::Global;
        assert!(!space.check(&SocketAddr::new(
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            8080
        )));
        assert!(!space.check(&SocketAddr::new(
            IpAddr::V6(Ipv6Addr::new(0xff00, 0, 0, 0, 0, 0, 0, 1)),
            8080
        )));
        assert!(!space.check(&SocketAddr::new(
            IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1)),
            8080
        )));
    }
}

