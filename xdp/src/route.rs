use {
    crate::netlink::{
        netlink_get_neighbors, netlink_get_routes, MacAddress, NeighborEntry, RouteEntry,
    },
    arc_swap::ArcSwap,
    libc::{AF_INET, AF_INET6},
    std::{
        io,
        net::{IpAddr, Ipv4Addr, Ipv6Addr},
        sync::Arc,
        time::Instant,
    },
    thiserror::Error,
};

#[derive(Debug, Error)]
pub enum RouteError {
    #[error("no route found to destination {0}")]
    NoRouteFound(IpAddr),

    #[error("missing output interface in route")]
    MissingOutputInterface,

    #[error("could not resolve MAC address")]
    MacResolutionError,
}

#[derive(Debug)]
pub struct NextHop {
    pub mac_addr: Option<MacAddress>,
    pub ip_addr: IpAddr,
    pub if_index: u32,
}

// Remove RouteUpdate enum - no longer needed with ArcSwap

fn lookup_route(routes: &[RouteEntry], dest: IpAddr) -> Option<&RouteEntry> {
    let mut best_match = None;

    let family = match dest {
        IpAddr::V4(_) => AF_INET as u8,
        IpAddr::V6(_) => AF_INET6 as u8,
    };

    for route in routes.iter().filter(|r| r.family == family) {
        match (dest, route.destination) {
            // this is the default route
            (_, None) => {
                if best_match.is_none() {
                    best_match = Some((route, 0));
                }
            }

            (IpAddr::V4(dest_addr), Some(IpAddr::V4(route_addr))) => {
                let prefix_len = route.dst_len;
                if !is_ipv4_match(dest_addr, route_addr, prefix_len) {
                    continue;
                }

                if best_match.is_none() || prefix_len > best_match.unwrap().1 {
                    best_match = Some((route, prefix_len));
                }
            }

            (IpAddr::V6(dest_addr), Some(IpAddr::V6(route_addr))) => {
                let prefix_len = route.dst_len;
                if !is_ipv6_match(dest_addr, route_addr, prefix_len) {
                    continue;
                }

                if best_match.is_none() || prefix_len > best_match.unwrap().1 {
                    best_match = Some((route, prefix_len));
                }
            }

            // mixed address families - can't match
            _ => continue,
        }
    }

    best_match.map(|(route, _)| route)
}

fn is_ipv4_match(addr: Ipv4Addr, network: Ipv4Addr, prefix_len: u8) -> bool {
    if prefix_len == 0 {
        return true;
    }

    let mask = 0xFFFFFFFF << 32u32.saturating_sub(prefix_len as u32);
    let addr_bits = u32::from(addr) & mask;
    let network_bits = u32::from(network) & mask;

    addr_bits == network_bits
}

fn is_ipv6_match(addr: Ipv6Addr, network: Ipv6Addr, prefix_len: u8) -> bool {
    if prefix_len == 0 {
        return true;
    }

    let addr_segments = addr.segments();
    let network_segments = network.segments();

    let full_segments = (prefix_len / 16) as usize;
    if addr_segments[..full_segments] != network_segments[..full_segments] {
        return false;
    }

    if let Some(remaining_bits) = prefix_len.checked_rem(16).filter(|&b| b != 0) {
        let mask = 0xFFFF_u16 << 16u16.saturating_sub(remaining_bits as u16);
        if (addr_segments[full_segments] & mask) != (network_segments[full_segments] & mask) {
            return false;
        }
    }

    true
}

#[derive(Clone)]
pub struct Router {
    routes: Arc<Vec<RouteEntry>>,
    arp_table: Arc<ArpTable>,
    last_update: Instant,
}

impl Router {
    pub fn new() -> Result<Self, io::Error> {
        let mut routes = netlink_get_routes(AF_INET as u8)?;
        filter_routes(&mut routes); // Apply filtering
        let arp_table = ArpTable::new()?;

        Ok(Self {
            routes: Arc::new(routes),
            arp_table: Arc::new(arp_table),
            last_update: Instant::now(),
        })
    }

    pub fn update_routes(&mut self, mut new_routes: Vec<RouteEntry>) {
        filter_routes(&mut new_routes); // Apply filtering
        self.routes = Arc::new(new_routes);
        self.last_update = Instant::now();
        log::debug!("greg: Updated routes: {} entries", self.routes.len());
    }

    pub fn update_arp(&mut self, new_neighbors: Vec<NeighborEntry>) {
        self.arp_table = Arc::new(ArpTable::from_neighbors(new_neighbors));
        self.last_update = Instant::now();
        log::debug!("greg: Updated ARP table: {} entries", self.arp_table.neighbors.len());
    }

    pub fn default(&self) -> Result<NextHop, RouteError> {
        let default_route = self
            .routes
            .iter()
            .find(|r| r.destination.is_none())
            .ok_or(RouteError::NoRouteFound(IpAddr::V4(Ipv4Addr::UNSPECIFIED)))?;

        let if_index = default_route
            .out_if_index
            .ok_or(RouteError::MissingOutputInterface)? as u32;

        let next_hop_ip = match default_route.gateway {
            Some(gateway) => gateway,
            None => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        };

        let mac_addr = self.arp_table.lookup(next_hop_ip).cloned();

        Ok(NextHop {
            ip_addr: next_hop_ip,
            mac_addr,
            if_index,
        })
    }

    // Fast route lookup. no locks
    pub fn route(&self, dest_ip: IpAddr) -> Result<NextHop, RouteError> {
        let route = lookup_route(&self.routes, dest_ip).ok_or(RouteError::NoRouteFound(dest_ip))?;

        let if_index = route
            .out_if_index
            .ok_or(RouteError::MissingOutputInterface)? as u32;

        let next_hop_ip = match route.gateway {
            Some(gateway) => gateway,
            None => dest_ip,
        };

        let mac_addr = self.arp_table.lookup(next_hop_ip).cloned();

        Ok(NextHop {
            ip_addr: next_hop_ip,
            mac_addr,
            if_index,
        })
    }

    // Get last update time for monitoring
    pub fn last_update(&self) -> Instant {
        self.last_update
    }
}

pub(crate) fn filter_routes(routes: &mut Vec<RouteEntry>) {
    routes.retain(|route| {
        // Keep only routes we care about
        match route.destination {
            Some(dest) => {
                // Keep specific network routes (not host routes from MTU discovery)
                // MTU discovery routes typically have /32 or /128 prefixes
                match dest {
                    IpAddr::V4(ipv4) => {
                        // Keep routes with prefix length <= 24 (subnet routes)
                        // Filter out /32 host routes (likely from MTU discovery)
                        route.dst_len <= 24 && !ipv4.is_loopback()
                    }
                    IpAddr::V6(ipv6) => {
                        // Keep routes with prefix length <= 64 (subnet routes)
                        // Filter out /128 host routes
                        route.dst_len <= 64 && !ipv6.is_loopback()
                    }
                }
            }
            None => {
                // Keep default routes (destination = None)
                true
            }
        }
    });
}

struct ArpTable {
    neighbors: Vec<NeighborEntry>,
}

impl ArpTable {
    pub fn new() -> Result<Self, io::Error> {
        let neighbors = netlink_get_neighbors(None, AF_INET as u8)?;
        Ok(Self { neighbors })
    }

    pub fn from_neighbors(neighbors: Vec<NeighborEntry>) -> Self {
        Self { neighbors }
    }

    pub fn lookup(&self, ip: IpAddr) -> Option<&MacAddress> {
        self.neighbors
            .iter()
            .find(|n| n.destination == Some(ip))
            .and_then(|n| n.lladdr.as_ref())
    }
}

pub struct AtomicRouter {
    router: ArcSwap<Router>,
}

impl AtomicRouter {
    pub fn new() -> Result<Self, io::Error> {
        Ok(Self {
            router: ArcSwap::from_pointee(Router::new()?),
        })
    }

    // Lock-free read - just load the current router
    pub fn load(&self) -> Arc<Router> {
        self.router.load().clone()
    }

    // Atomic update - store new router
    pub fn update_routes(&self, new_routes: Vec<RouteEntry>) {
        let mut current_router = (**self.router.load()).clone();
        current_router.update_routes(new_routes);
        self.router.store(Arc::new(current_router));
    }

    // Atomic update - store new router
    pub fn update_arp(&self, new_neighbors: Vec<NeighborEntry>) {
        let mut current_router = (**self.router.load()).clone();
        current_router.update_arp(new_neighbors);
        self.router.store(Arc::new(current_router));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ipv4_match() {
        assert!(is_ipv4_match(
            Ipv4Addr::new(192, 168, 1, 10),
            Ipv4Addr::new(192, 168, 1, 0),
            24
        ));

        assert!(!is_ipv4_match(
            Ipv4Addr::new(192, 168, 2, 10),
            Ipv4Addr::new(192, 168, 1, 0),
            24
        ));

        // Match with default route
        assert!(is_ipv4_match(
            Ipv4Addr::new(1, 2, 3, 4),
            Ipv4Addr::new(0, 0, 0, 0),
            0
        ));
    }

    #[test]
    fn test_ipv6_match() {
        assert!(is_ipv6_match(
            Ipv6Addr::new(0x2001, 0xdb8, 0x1234, 0x5678, 0xabcd, 0xef01, 0x2345, 0x6789),
            Ipv6Addr::new(0x2001, 0xdb8, 0x1234, 0x5678, 0, 0, 0, 0),
            64
        ));

        assert!(!is_ipv6_match(
            Ipv6Addr::new(0x2001, 0xdb8, 0x1235, 0x5678, 0xabcd, 0xef01, 0x2345, 0x6789),
            Ipv6Addr::new(0x2001, 0xdb8, 0x1234, 0x5678, 0, 0, 0, 0),
            64
        ));

        // Match with partial segment
        assert!(is_ipv6_match(
            Ipv6Addr::new(0x2001, 0xdb8, 0x1234, 0x6700, 0, 0, 0, 0),
            Ipv6Addr::new(0x2001, 0xdb8, 0x1234, 0x6600, 0, 0, 0, 0),
            52
        ));

        assert!(!is_ipv6_match(
            Ipv6Addr::new(0x2001, 0xdb8, 0x1234, 0x6700, 0, 0, 0, 0),
            Ipv6Addr::new(0x2001, 0xdb8, 0x1234, 0x5600, 0, 0, 0, 0),
            52
        ));
    }

    // #[test]
    // fn test_router() {
    //     let router = Router::new().unwrap();
    //     let next_hop = router.route("1.1.1.1".parse().unwrap()).unwrap();
    //     eprintln!("{next_hop:?}");
    // }

    #[test]
    fn test_route_cache() {
        let router = Router::new().unwrap();
        let next_hop = router.route("1.1.1.1".parse().unwrap()).unwrap();
        eprintln!("{next_hop:?}");
    }
}
