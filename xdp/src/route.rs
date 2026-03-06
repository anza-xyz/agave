use {
    crate::netlink::{
        netlink_get_interfaces, netlink_get_neighbors, netlink_get_routes, GreTunnelInfo,
        InterfaceInfo, MacAddress, NeighborEntry, RouteEntry,
    },
    libc::{AF_INET, AF_INET6},
    std::{
        io,
        net::{IpAddr, Ipv4Addr, Ipv6Addr},
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

    #[error("unknown interface index {0}")]
    UnknownInterfaceIndex(u32),
}

#[derive(Debug, Clone)]
pub struct GreRouteInfo {
    pub if_index: u32,
    pub tunnel_info: GreTunnelInfo,
    pub mac_addr: MacAddress,
}

#[derive(Debug, Clone)]
pub struct NextHop {
    pub mac_addr: Option<MacAddress>,
    pub ip_addr: IpAddr,
    pub if_index: u32,
    pub preferred_src_ip: Option<Ipv4Addr>,
    pub gre: Option<GreRouteInfo>,
}

fn lookup_route<'a, I>(routes: I, dest: IpAddr) -> Option<&'a RouteEntry>
where
    I: Iterator<Item = &'a RouteEntry>,
{
    let mut best_match = None;

    let family = match dest {
        IpAddr::V4(_) => AF_INET as u8,
        IpAddr::V6(_) => AF_INET6 as u8,
    };

    for route in routes.filter(|r| r.family == family) {
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
struct RouteTable {
    routes: Vec<RouteEntry>,
}

impl RouteTable {
    pub fn new() -> Result<Self, io::Error> {
        let routes = netlink_get_routes(AF_INET as u8)?;
        Ok(Self { routes })
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &RouteEntry> {
        self.routes.iter()
    }

    pub fn upsert(&mut self, new_route: RouteEntry) -> bool {
        if let Some(existing) = self.routes.iter_mut().find(|old| old.same_key(&new_route)) {
            if existing != &new_route {
                *existing = new_route;
                return true;
            }
            false
        } else {
            self.routes.push(new_route);
            true
        }
    }

    pub fn remove(&mut self, new_route: RouteEntry) -> bool {
        if let Some(i) = self.routes.iter().position(|old| old.same_key(&new_route)) {
            self.routes.swap_remove(i);
            return true;
        }
        false
    }
}

#[derive(Clone, Debug)]
struct InterfaceTable {
    interfaces: Vec<InterfaceInfo>,
}

impl InterfaceTable {
    pub fn new() -> Result<Self, io::Error> {
        Ok(Self {
            interfaces: netlink_get_interfaces(AF_INET as u8)?.into_iter().collect(),
        })
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &InterfaceInfo> {
        self.interfaces.iter()
    }

    pub fn upsert(&mut self, new_interface: InterfaceInfo) -> bool {
        if let Some(existing) = self
            .interfaces
            .iter_mut()
            .find(|old| old.if_index == new_interface.if_index)
        {
            if existing != &new_interface {
                *existing = new_interface;
                return true;
            }
            return false;
        }
        self.interfaces.push(new_interface);
        true
    }

    pub fn remove(&mut self, if_index: u32) -> bool {
        if let Some(i) = self
            .interfaces
            .iter()
            .position(|old| old.if_index == if_index)
        {
            self.interfaces.swap_remove(i);
            return true;
        }
        false
    }
}

#[derive(Clone)]
pub struct Router {
    arp_table: ArpTable,
    route_table: RouteTable,
    interface_table: InterfaceTable,
    // cache for the default route next hop so we can avoid arp table lookups on the common case
    cached_default_route: Option<NextHop>,
    // cache for gre route info to avoid repeated lookups in the common case
    // where there is only one gre interface
    cached_gre_info: Option<GreRouteInfo>,
}

impl Router {
    pub fn new() -> Result<Self, io::Error> {
        Ok(Self {
            arp_table: ArpTable::new()?,
            route_table: RouteTable::new()?,
            interface_table: InterfaceTable::new()?,
            cached_default_route: None,
            cached_gre_info: None,
        })
    }

    fn default_route(&self) -> Result<NextHop, RouteError> {
        let default_route = self
            .route_table
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

        let mac_addr = self.arp_table.lookup(next_hop_ip, if_index).cloned();
        let preferred_src_ip = match default_route.pref_src {
            Some(IpAddr::V4(v4)) => Some(v4),
            _ => None,
        };

        let gre = self
            .interface_table
            .iter()
            .find(|i| i.if_index == if_index)
            .and_then(|interface| self.interface_gre_route_info(interface));

        Ok(NextHop {
            ip_addr: next_hop_ip,
            mac_addr,
            if_index,
            preferred_src_ip,
            gre,
        })
    }

    pub fn default(&self) -> Result<NextHop, RouteError> {
        if let Some(default_route) = &self.cached_default_route {
            Ok(default_route.clone())
        } else {
            self.default_route()
        }
    }

    pub fn route(&self, dest_ip: IpAddr) -> Result<NextHop, RouteError> {
        let route = lookup_route(self.route_table.iter(), dest_ip)
            .ok_or(RouteError::NoRouteFound(dest_ip))?;

        let if_index = route
            .out_if_index
            .ok_or(RouteError::MissingOutputInterface)? as u32;

        let next_hop_ip = match route.gateway {
            Some(gateway) => gateway,
            None => dest_ip,
        };

        let preferred_src_ip = match route.pref_src {
            Some(IpAddr::V4(v4)) => Some(v4),
            _ => None,
        };

        if let Some(default_route) = &self.cached_default_route {
            if default_route.ip_addr == next_hop_ip && default_route.if_index == if_index {
                return Ok(NextHop {
                    ip_addr: next_hop_ip,
                    if_index,
                    mac_addr: default_route.mac_addr,
                    preferred_src_ip,
                    gre: default_route.gre.clone(),
                });
            }
        }

        if let Some(gre) = &self.cached_gre_info {
            if gre.if_index == if_index {
                return Ok(NextHop {
                    if_index,
                    ip_addr: next_hop_ip,
                    mac_addr: Some(gre.mac_addr),
                    preferred_src_ip,
                    gre: Some(gre.clone()),
                });
            }
        }

        let mac_addr = self.arp_table.lookup(next_hop_ip, if_index).cloned();

        let next_hop = NextHop {
            ip_addr: next_hop_ip,
            mac_addr,
            if_index,
            preferred_src_ip,
            gre: None,
        };
        Ok(next_hop)
    }

    // called to rebuild cached values after route/neigh/interface updates right
    // before a new Router instance is published
    pub fn build_caches(&mut self) -> Result<(), io::Error> {
        self.cached_default_route = None;
        self.cached_gre_info = None;

        let mut has_gre_interface = false;
        for interface in self.interface_table.iter() {
            if interface.gre_tunnel.is_some() {
                has_gre_interface = true;
                if self.cached_gre_info.is_none() {
                    self.cached_gre_info = self.interface_gre_route_info(interface);
                }
            }
        }
        if self.cached_gre_info.is_none() && has_gre_interface {
            log::warn!("GRE cache: GRE interface(s) present but none with valid remote resolved");
        }
        self.cached_default_route = match self.default_route() {
            Ok(hop) => Some(hop),
            Err(RouteError::NoRouteFound(_)) => None,
            Err(e) => return Err(io::Error::other(e)),
        };

        Ok(())
    }

    fn interface_gre_route_info(&self, interface: &InterfaceInfo) -> Option<GreRouteInfo> {
        let tunnel_info = interface.gre_tunnel.as_ref()?;
        let remote = tunnel_info.remote;
        let local = tunnel_info.local;
        // Skip unconfigured tunnels (remote/local 0.0.0.0)
        if remote == IpAddr::V4(Ipv4Addr::UNSPECIFIED) || local == IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        {
            return None;
        }
        // ARP for the GRE remote is on the egress interface. Caches must be cleared
        // at the start of build_caches() for route() to be safe.
        debug_assert!(self.cached_gre_info.is_none());
        let next_hop = self.route(remote).ok()?;
        let mac_addr = next_hop.mac_addr?;

        Some(GreRouteInfo {
            if_index: interface.if_index,
            tunnel_info: tunnel_info.clone(),
            mac_addr,
        })
    }

    pub fn upsert_route(&mut self, new_route: RouteEntry) -> bool {
        self.route_table.upsert(new_route)
    }

    pub fn remove_route(&mut self, new_route: RouteEntry) -> bool {
        self.route_table.remove(new_route)
    }

    pub fn upsert_neighbor(&mut self, new_neighbor: NeighborEntry) -> bool {
        self.arp_table.upsert(new_neighbor)
    }

    pub fn remove_neighbor(&mut self, ip: Ipv4Addr, if_index: u32) -> bool {
        self.arp_table.remove(ip, if_index)
    }

    pub fn upsert_interface(&mut self, new_interface: InterfaceInfo) -> bool {
        self.interface_table.upsert(new_interface)
    }

    pub fn remove_interface(&mut self, if_index: u32) -> bool {
        self.interface_table.remove(if_index)
    }
}

#[derive(Clone)]
struct ArpTable {
    neighbors: Vec<NeighborEntry>,
}

impl ArpTable {
    pub fn new() -> Result<Self, io::Error> {
        let neighbors = netlink_get_neighbors(None, AF_INET as u8)?;
        Ok(Self { neighbors })
    }

    pub fn lookup(&self, ip: IpAddr, if_index: u32) -> Option<&MacAddress> {
        self.neighbors
            .iter()
            .find(|n| n.ifindex == if_index as i32 && n.destination == Some(ip))
            .and_then(|n| n.lladdr.as_ref())
    }

    pub fn upsert(&mut self, new_neighbor: NeighborEntry) -> bool {
        let Some((ifidx, ip)) = new_neighbor.key() else {
            return false;
        };

        if let Some(i) = self
            .neighbors
            .iter()
            .position(|old| old.ifindex == ifidx && old.destination == Some(IpAddr::V4(ip)))
        {
            if self.neighbors[i] != new_neighbor {
                self.neighbors[i] = new_neighbor;
                return true;
            }
            false
        } else {
            self.neighbors.push(new_neighbor);
            true
        }
    }

    pub fn remove(&mut self, ip: Ipv4Addr, if_index: u32) -> bool {
        if let Some(i) = self.neighbors.iter().position(|old| {
            old.ifindex == if_index as i32 && old.destination == Some(IpAddr::V4(ip))
        }) {
            self.neighbors.swap_remove(i);
            return true;
        }
        false
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
        let mut router = Router::new().unwrap();
        let next_hop = router.route("1.1.1.1".parse().unwrap()).unwrap();
        eprintln!("{next_hop:?}");

        let before_routes_len = router.route_table.iter().len();

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
        assert!(router.upsert_route(route.clone()));
        assert!(router.route_table.iter().any(|r| r == &route));
        assert!(router.route_table.iter().len() >= before_routes_len);

        // Delete using same key should remove the route
        assert!(router.remove_route(route.clone()));
        assert!(router.route_table.iter().all(|r| r != &route));
        assert_eq!(router.route_table.iter().len(), before_routes_len);
    }

    #[test]
    fn test_arp_table() {
        let mut router = Router::new().unwrap();
        let before_neigh_len = router.arp_table.neighbors.len();

        // Create a unique, private neighbor entry on a dummy ifindex
        let neigh_ip = Ipv4Addr::new(10, 255, 255, 77);
        let entry = NeighborEntry {
            destination: Some(IpAddr::V4(neigh_ip)),
            lladdr: Some(MacAddress([0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0x01])),
            ifindex: 1,
            state: NUD_REACHABLE,
        };

        // Upsert new neighbor and check that it was inserted and neighbors are dirty
        assert!(router.upsert_neighbor(entry.clone()));
        assert!(router.arp_table.neighbors.iter().any(|n| n == &entry));
        assert!(router.arp_table.neighbors.len() >= before_neigh_len);

        // Delete neighbor and check that it was deleted
        assert!(router.remove_neighbor(neigh_ip, 1));
        assert!(router.arp_table.neighbors.iter().all(|n| n != &entry));
        assert_eq!(router.arp_table.neighbors.len(), before_neigh_len);
    }

    #[test]
    fn test_interface_table() {
        let mut router = Router::new().unwrap();
        let before_interface_len = router.interface_table.iter().len();

        // Create a unique, private interface with a dummy ifindex
        let test_if_index = 99999;
        let interface = InterfaceInfo {
            if_index: test_if_index,
            gre_tunnel: None,
        };

        // Upsert new interface and check that it was inserted
        assert!(router.upsert_interface(interface.clone()));
        assert!(router.interface_table.iter().any(|i| i == &interface));
        assert!(router.interface_table.iter().len() >= before_interface_len);

        // Upsert same interface with no changes should return false
        assert!(!router.upsert_interface(interface.clone()));

        // Upsert with changes should return true
        let mut modified_interface = interface.clone();
        modified_interface.gre_tunnel = Some(GreTunnelInfo {
            local: IpAddr::V4(Ipv4Addr::new(10, 255, 255, 2)),
            remote: IpAddr::V4(Ipv4Addr::new(10, 255, 255, 1)),
            ttl: 0,
            tos: 0,
            pmtudisc: 0,
        });
        assert!(router.upsert_interface(modified_interface.clone()));
        assert!(router
            .interface_table
            .iter()
            .any(|i| i == &modified_interface));
        assert!(router.interface_table.iter().all(|i| i != &interface));

        // Delete interface and check that it was deleted
        assert!(router.remove_interface(test_if_index));
        assert!(router
            .interface_table
            .iter()
            .all(|i| i.if_index != test_if_index));
        assert_eq!(router.interface_table.iter().len(), before_interface_len);
    }
}
