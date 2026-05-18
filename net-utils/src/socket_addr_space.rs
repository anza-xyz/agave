use std::net::{SocketAddr, SocketAddrV4};

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
    pub fn check_v4(&self, addr: &SocketAddrV4) -> bool {
        if matches!(self, SocketAddrSpace::Unspecified) {
            return true;
        }

        let addr = addr.ip();
        // TODO: Consider excluding:
        // addr.is_link_local() || addr.is_broadcast()
        // || addr.is_documentation() || addr.is_unspecified()
        !(addr.is_private() || addr.is_loopback())
    }

    /// Returns true if the IP address is valid.
    #[inline]
    #[must_use]
    pub fn check(&self, addr: &SocketAddr) -> bool {
        if matches!(self, SocketAddrSpace::Unspecified) {
            return true;
        }

        // TODO: remove these once IpAddr::is_global is stable.
        match addr {
            SocketAddr::V4(addr) => self.check_v4(addr),
            SocketAddr::V6(addr) => {
                // TODO: Consider excluding:
                // addr.is_unspecified(),
                !addr.ip().is_loopback()
            }
        }
    }
}
