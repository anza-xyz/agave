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
    arp_table: Arc<ArpTable>,
    routes: Arc<Vec<RouteEntry>>,
}

impl Router {
    pub fn new() -> Result<Self, io::Error> {
        let arp_table = ArpTable::new()?;
        let routes = netlink_get_routes(AF_INET as u8)?;

        Ok(Self {
            arp_table: Arc::new(arp_table),
            routes: Arc::new(routes),
        })
    }

    pub fn clone_neighbors(&self) -> Vec<NeighborEntry> {
        self.arp_table.neighbors.clone()
    }

    pub fn clone_routes(&self) -> Vec<RouteEntry> {
        self.routes.as_ref().clone()
    }

    pub fn default(&self) -> Result<NextHop, RouteError> {
        let default_route = self
            .routes
            .iter()
            .find(|r| r.destination.is_none())
            .ok_or(RouteError::NoRouteFound(IpAddr::V4(Ipv4Addr::UNSPECIFIED)))?;

        let if_index = default_route
            .out_if_index
            .ok_or(RouteError::MissingOutputInterface)?;

        let next_hop_ip = match default_route.gateway {
            Some(gateway) => gateway,
            None => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        };

        let mac_addr = self.arp_table.lookup(next_hop_ip, if_index).cloned();

        Ok(NextHop {
            ip_addr: next_hop_ip,
            mac_addr,
            if_index: if_index as u32,
        })
    }

    pub fn route(&self, dest_ip: IpAddr) -> Result<NextHop, RouteError> {
        let route = lookup_route(&self.routes, dest_ip).ok_or(RouteError::NoRouteFound(dest_ip))?;

        let if_index = route
            .out_if_index
            .ok_or(RouteError::MissingOutputInterface)?;

        let next_hop_ip = match route.gateway {
            Some(gateway) => gateway,
            None => dest_ip,
        };

        let mac_addr = self.arp_table.lookup(next_hop_ip, if_index).cloned();

        Ok(NextHop {
            ip_addr: next_hop_ip,
            mac_addr,
            if_index: if_index as u32,
        })
    }
}

#[derive(Clone)]
pub(crate) struct ArpTable {
    pub(crate) neighbors: Vec<NeighborEntry>,
}

impl ArpTable {
    pub fn new() -> Result<Self, io::Error> {
        let neighbors = netlink_get_neighbors(None, AF_INET as u8)?;
        Ok(Self { neighbors })
    }

    pub fn lookup(&self, ip: IpAddr, if_index: i32) -> Option<&MacAddress> {
        self.neighbors
            .iter()
            .find(|n| n.ifindex == if_index && n.destination == Some(ip))
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

    pub fn load(&self) -> Arc<Router> {
        self.router.load().clone()
    }

    /// update both routes and ARP table
    pub fn resync(&self) -> Result<(), io::Error> {
        let mut current_router = (**self.router.load()).clone();
        current_router.routes = Arc::new(netlink_get_routes(AF_INET as u8)?);
        current_router.arp_table = Arc::new(ArpTable {
            neighbors: netlink_get_neighbors(None, AF_INET as u8)?,
        });
        self.router.store(Arc::new(current_router));
        Ok(())
    }

    pub fn publish_snapshot(&self, working: &WorkingRouter) {
        let router = Router {
            arp_table: Arc::new(ArpTable {
                neighbors: working.neigh.clone(),
            }),
            routes: Arc::new(working.routes.clone()),
        };
        self.router.store(Arc::new(router));
    }
}

// Working Router used for lock-free updates
pub struct WorkingRouter {
    routes: Vec<RouteEntry>,
    neigh: Vec<NeighborEntry>,
    dirty_routes: bool,
    dirty_neigh: bool,
}

impl WorkingRouter {
    // create a working router from the atomic router
    // only called on startup and when the atomic router is resynced due to a netlink error
    pub fn from_atomic_router(router: &AtomicRouter) -> Self {
        let router = router.load();
        let mut routes = router.clone_routes();
        let mut neigh = router.clone_neighbors();
        routes.reserve(routes.len().saturating_mul(2).max(512));
        neigh.reserve(neigh.len().saturating_mul(2).max(128));
        Self {
            routes,
            neigh,
            dirty_routes: false,
            dirty_neigh: false,
        }
    }

    pub fn dirty_routes(&self) -> bool {
        self.dirty_routes
    }

    pub fn dirty_neigh(&self) -> bool {
        self.dirty_neigh
    }

    pub fn clear_dirty(&mut self) {
        self.dirty_routes = false;
        self.dirty_neigh = false;
    }

    #[inline]
    fn same_key(a: &RouteEntry, b: &RouteEntry) -> bool {
        a.family == b.family
            && a.dst_len == b.dst_len
            && a.destination == b.destination
            && a.table == b.table
            && a.type_ == b.type_
    }

    #[inline]
    fn neighbor_key(n: &NeighborEntry) -> Option<(i32, Ipv4Addr)> {
        match n.destination {
            Some(IpAddr::V4(ip)) => Some((n.ifindex, ip)),
            _ => None,
        }
    }

    pub fn upsert_route(&mut self, new_route: RouteEntry) {
        if let Some(i) = self
            .routes
            .iter()
            .position(|old| Self::same_key(old, &new_route))
        {
            if self.routes[i] != new_route {
                self.routes[i] = new_route;
                self.dirty_routes = true;
            }
        } else {
            self.routes.push(new_route);
            self.dirty_routes = true;
        }
    }

    pub fn delete_route(&mut self, new_route: RouteEntry) {
        if let Some(i) = self
            .routes
            .iter()
            .position(|old| Self::same_key(old, &new_route))
        {
            self.routes.swap_remove(i);
            self.dirty_routes = true;
        }
    }

    pub fn upsert_neighbor(&mut self, new_neighbor: NeighborEntry) {
        if !new_neighbor.is_valid() {
            return;
        }
        let Some((ifidx, ip)) = Self::neighbor_key(&new_neighbor) else {
            return;
        };

        if let Some(i) = self
            .neigh
            .iter()
            .position(|old| old.ifindex == ifidx && old.destination == Some(IpAddr::V4(ip)))
        {
            if self.neigh[i] != new_neighbor {
                self.neigh[i] = new_neighbor;
                self.dirty_neigh = true;
            }
        } else {
            self.neigh.push(new_neighbor);
            self.dirty_neigh = true;
        }
    }

    pub fn delete_neighbor(&mut self, ip: Ipv4Addr, if_index: i32) {
        if let Some(i) = self
            .neigh
            .iter()
            .position(|old| old.ifindex == if_index && old.destination == Some(IpAddr::V4(ip)))
        {
            self.neigh.swap_remove(i);
            self.dirty_neigh = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::netlink::{MacAddress, NeighborEntry, RouteEntry},
        libc::{AF_INET, NUD_REACHABLE},
        std::net::{IpAddr, Ipv4Addr},
    };

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

    #[test]
    fn test_router() {
        let router = Router::new().unwrap();
        let next_hop = router.route("1.1.1.1".parse().unwrap()).unwrap();
        eprintln!("{next_hop:?}");
    }

    #[test]
    fn test_working_upsert_and_delete_route() {
        let atomic_router = AtomicRouter::new().unwrap();
        let router_before = atomic_router.load();
        let before_routes = router_before.clone_routes();

        let mut working = WorkingRouter::from_atomic_router(&atomic_router);

        // Create a unique, private IPv4 /32 route to avoid collisions
        let test_dst = Ipv4Addr::new(10, 255, 255, 123);
        let route = RouteEntry {
            destination: Some(IpAddr::V4(test_dst)),
            gateway: Some(IpAddr::V4(Ipv4Addr::new(10, 255, 255, 1))),
            pref_src: None,
            out_if_index: Some(1),
            in_if_index: None,
            priority: None,
            table: None,
            protocol: 0,
            scope: 0,
            type_: 0,
            family: AF_INET as u8,
            dst_len: 32,
            flags: 0,
        };

        // Upsert new route and check that it was inserted and routes are dirty
        working.upsert_route(route.clone());
        assert!(working.dirty_routes());
        atomic_router.publish_snapshot(&working);
        working.clear_dirty();
        assert!(!working.dirty_routes());

        let router_after_insert = atomic_router.load();
        let after_insert_routes = router_after_insert.clone_routes();
        assert!(after_insert_routes.iter().any(|r| r == &route));
        assert!(after_insert_routes.len() >= before_routes.len());

        // Delete using same key should remove the route
        working.delete_route(route.clone());
        assert!(working.dirty_routes());
        atomic_router.publish_snapshot(&working);
        working.clear_dirty();

        let router_after_delete = atomic_router.load();
        let after_delete_routes = router_after_delete.clone_routes();
        assert!(after_delete_routes.iter().all(|r| r != &route));
        assert!(after_delete_routes.len() == before_routes.len());
    }

    #[test]
    fn test_working_upsert_and_delete_neighbor() {
        let atomic_router = AtomicRouter::new().unwrap();
        let router_before = atomic_router.load();
        let before_neigh = router_before.clone_neighbors();

        let mut working = WorkingRouter::from_atomic_router(&atomic_router);

        // Create a unique, private neighbor entry on a dummy ifindex
        let neigh_ip = Ipv4Addr::new(10, 255, 255, 77);
        let entry = NeighborEntry {
            destination: Some(IpAddr::V4(neigh_ip)),
            lladdr: Some(MacAddress([0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0x01])),
            ifindex: 1,
            state: NUD_REACHABLE,
        };

        // Upsert new neighbor and check that it was inserted and neighbors are dirty
        working.upsert_neighbor(entry.clone());
        assert!(working.dirty_neigh());
        atomic_router.publish_snapshot(&working);
        working.clear_dirty();
        assert!(!working.dirty_neigh());

        let router_after_insert = atomic_router.load();
        let after_insert_neigh = router_after_insert.clone_neighbors();
        assert!(after_insert_neigh.iter().any(|n| n == &entry));
        assert!(after_insert_neigh.len() >= before_neigh.len());

        // Delete neighbor and check that it was deleted
        working.delete_neighbor(neigh_ip, 1);
        assert!(working.dirty_neigh());
        atomic_router.publish_snapshot(&working);
        working.clear_dirty();

        let router_after_delete = atomic_router.load();
        let after_delete_neigh = router_after_delete.clone_neighbors();
        assert!(after_delete_neigh.iter().all(|n| n != &entry));
        assert!(after_delete_neigh.len() == before_neigh.len());
    }
}
