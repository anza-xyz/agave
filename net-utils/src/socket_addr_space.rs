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
        if matches!(self, SocketAddrSpace::Unspecified) {
            return true;
        }

        // TODO: remove these once IpAddr::is_global is stable.
        match addr.ip() {
            IpAddr::V4(addr) => {
                // TODO: Consider excluding:
                // addr.is_link_local() || addr.is_broadcast()
                // || addr.is_documentation() || addr.is_unspecified()
                !(addr.is_private() || addr.is_loopback())
            }
            IpAddr::V6(addr) => {
                // TODO: Consider excluding:
                // addr.is_unspecified(),
                !addr.is_loopback()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        std::net::{IpAddr, Ipv6Addr},
    };

    #[test]
    fn test_socket_addr_space_new() {
        assert_eq!(SocketAddrSpace::new(true), SocketAddrSpace::Unspecified);
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
    fn test_ipv6_addresses() {
        let space = SocketAddrSpace::Global;

        assert!(!space.check(&SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 8080)));

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
