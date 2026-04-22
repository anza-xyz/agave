use {
    crate::{
        lpm::Ipv4Lpm,
        netlink::{
            GreTunnelInfo, InterfaceInfo, MacAddress, NeighborEntry, RouteEntry,
            netlink_get_interfaces, netlink_get_neighbors, netlink_get_routes,
        },
    },
    libc::{AF_INET, RT_TABLE_DEFAULT, RT_TABLE_LOCAL, RT_TABLE_MAIN},
    log::warn,
    std::{
        cmp::Ordering,
        fmt, io,
        net::{IpAddr, Ipv4Addr},
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteTable {
    Default,
    Local,
    Main,
    Other(u32),
}

impl std::fmt::Display for RouteTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Default => f.write_str("default"),
            Self::Local => f.write_str("local"),
            Self::Main => f.write_str("main"),
            Self::Other(table) => write!(f, "{table}"),
        }
    }
}

impl From<RouteTable> for u32 {
    fn from(table: RouteTable) -> Self {
        match table {
            RouteTable::Default => u32::from(RT_TABLE_DEFAULT),
            RouteTable::Local => u32::from(RT_TABLE_LOCAL),
            RouteTable::Main => u32::from(RT_TABLE_MAIN),
            RouteTable::Other(table) => table,
        }
    }
}

impl From<u32> for RouteTable {
    fn from(table: u32) -> Self {
        match table {
            table if table == u32::from(RT_TABLE_DEFAULT) => Self::Default,
            table if table == u32::from(RT_TABLE_LOCAL) => Self::Local,
            table if table == u32::from(RT_TABLE_MAIN) => Self::Main,
            table => Self::Other(table),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route<T> {
    pub destination: Option<T>,
    pub gateway: Option<T>,
    pub preferred_src: Option<T>,
    pub out_if_index: Option<u32>,
    pub priority: Option<u32>,
    pub type_: u8,
    pub dst_len: u8,
}

impl<T: PartialEq> Route<T> {
    fn same_key(&self, other: &Self) -> bool {
        self.destination == other.destination
            && self.dst_len == other.dst_len
            && self.priority == other.priority
            && self.type_ == other.type_
    }
}

impl TryFrom<RouteEntry> for Route<Ipv4Addr> {
    type Error = ();

    fn try_from(entry: RouteEntry) -> Result<Self, Self::Error> {
        if entry.family != AF_INET as u8 {
            return Err(());
        }

        let destination = match entry.destination {
            Some(IpAddr::V4(addr)) => Some(addr),
            Some(IpAddr::V6(_)) => return Err(()),
            None => None,
        };
        let gateway = match entry.gateway {
            Some(IpAddr::V4(addr)) => Some(addr),
            Some(IpAddr::V6(_)) => return Err(()),
            None => None,
        };
        let preferred_src = match entry.pref_src {
            Some(IpAddr::V4(addr)) => Some(addr),
            Some(IpAddr::V6(_)) => return Err(()),
            None => None,
        };
        let out_if_index = entry
            .out_if_index
            .map(u32::try_from)
            .transpose()
            .map_err(|_| ())?;

        Ok(Self {
            destination,
            gateway,
            preferred_src,
            out_if_index,
            priority: entry.priority,
            type_: entry.type_,
            dst_len: entry.dst_len.min(32),
        })
    }
}

#[derive(Clone, Debug)]
pub struct Interfaces {
    interfaces: Vec<InterfaceInfo>,
}

impl Interfaces {
    pub fn new(interfaces: Vec<InterfaceInfo>) -> Self {
        Self { interfaces }
    }

    pub fn from_netlink() -> Result<Self, io::Error> {
        Ok(Self::new(netlink_get_interfaces(AF_INET as u8)?))
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &InterfaceInfo> {
        self.interfaces.iter()
    }

    fn upsert(&mut self, new_interface: InterfaceInfo) -> bool {
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

    fn remove(&mut self, if_index: u32) -> bool {
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
pub struct Neighbors {
    neighbors: Vec<NeighborEntry>,
}

impl Neighbors {
    pub fn new(neighbors: Vec<NeighborEntry>) -> Self {
        Self { neighbors }
    }

    pub fn from_netlink() -> Result<Self, io::Error> {
        Ok(Self::new(netlink_get_neighbors(None, AF_INET as u8)?))
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &NeighborEntry> {
        self.neighbors.iter()
    }

    fn lookup(&self, ip: IpAddr, if_index: u32) -> Option<&MacAddress> {
        self.neighbors
            .iter()
            .find(|n| n.ifindex == if_index as i32 && n.destination == Some(ip))
            .and_then(|n| n.lladdr.as_ref())
    }

    fn upsert(&mut self, new_neighbor: NeighborEntry) -> bool {
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

    fn remove(&mut self, ip: Ipv4Addr, if_index: u32) -> bool {
        if let Some(i) = self.neighbors.iter().position(|old| {
            old.ifindex == if_index as i32 && old.destination == Some(IpAddr::V4(ip))
        }) {
            self.neighbors.swap_remove(i);
            return true;
        }
        false
    }
}

#[derive(Clone)]
pub struct Routes {
    pub(crate) table: RouteTable,
    routes: Vec<Route<Ipv4Addr>>,
}

impl Routes {
    pub fn new(table: RouteTable, mut routes: Vec<Route<Ipv4Addr>>) -> Self {
        routes.sort_by(Self::compare_routes);
        Self { table, routes }
    }

    pub fn from_netlink(table: RouteTable) -> Result<Self, io::Error> {
        let routes = netlink_get_routes(AF_INET as u8, u32::from(table))?
            .into_iter()
            .filter_map(|entry| Route::try_from(entry).ok())
            .collect();
        Ok(Self::new(table, routes))
    }

    fn compare_routes(left: &Route<Ipv4Addr>, right: &Route<Ipv4Addr>) -> Ordering {
        right
            .dst_len
            .cmp(&left.dst_len)
            .then_with(|| left.priority.unwrap_or(0).cmp(&right.priority.unwrap_or(0)))
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &Route<Ipv4Addr>> {
        self.routes.iter()
    }

    fn get(&self, route_idx: usize) -> Option<&Route<Ipv4Addr>> {
        self.routes.get(route_idx)
    }

    fn as_slice(&self) -> &[Route<Ipv4Addr>] {
        &self.routes
    }

    fn upsert(&mut self, new_route: RouteEntry) -> bool {
        if !new_route
            .table
            .is_some_and(|table| self.table == RouteTable::from(table))
        {
            return false;
        }

        let Ok(new_route) = Route::try_from(new_route) else {
            return false;
        };

        if let Some(existing) = self.routes.iter_mut().find(|old| old.same_key(&new_route)) {
            if existing != &new_route {
                *existing = new_route;
                return true;
            }
            false
        } else {
            let route = new_route;
            let insert_at = self.routes.partition_point(|existing| {
                matches!(
                    Self::compare_routes(existing, &route),
                    Ordering::Less | Ordering::Equal
                )
            });
            self.routes.insert(insert_at, route);
            true
        }
    }

    fn remove(&mut self, new_route: RouteEntry) -> bool {
        if !new_route
            .table
            .is_some_and(|table| self.table == RouteTable::from(table))
        {
            return false;
        }

        let Ok(new_route) = Route::try_from(new_route) else {
            return false;
        };

        if let Some(i) = self.routes.iter().position(|old| old.same_key(&new_route)) {
            self.routes.remove(i);
            true
        } else {
            false
        }
    }
}

#[derive(Clone)]
pub struct RoutingTables {
    pub(crate) routes: Routes,
    neighbors: Neighbors,
    interfaces: Interfaces,
}

impl RoutingTables {
    pub fn new(routes: Routes, neighbors: Neighbors, interfaces: Interfaces) -> Self {
        Self {
            routes,
            neighbors,
            interfaces,
        }
    }

    pub fn from_netlink(table: RouteTable) -> Result<Self, io::Error> {
        Ok(Self::new(
            Routes::from_netlink(table)?,
            Neighbors::from_netlink()?,
            Interfaces::from_netlink()?,
        ))
    }

    pub fn upsert_route(&mut self, new_route: RouteEntry) -> bool {
        self.routes.upsert(new_route)
    }

    pub fn remove_route(&mut self, new_route: RouteEntry) -> bool {
        self.routes.remove(new_route)
    }

    pub fn upsert_neighbor(&mut self, new_neighbor: NeighborEntry) -> bool {
        self.neighbors.upsert(new_neighbor)
    }

    pub fn remove_neighbor(&mut self, ip: Ipv4Addr, if_index: u32) -> bool {
        self.neighbors.remove(ip, if_index)
    }

    pub fn upsert_interface(&mut self, new_interface: InterfaceInfo) -> bool {
        self.interfaces.upsert(new_interface)
    }

    pub fn remove_interface(&mut self, if_index: u32) -> bool {
        self.interfaces.remove(if_index)
    }
}

#[derive(Clone)]
pub struct Router {
    neighbors: Neighbors,
    routes: Routes,
    ipv4_lpm: Ipv4Lpm,
    interfaces: Interfaces,
    // cache for the default route next hop so we can avoid arp table lookups on the common case
    cached_default_route: Option<NextHop>,
    // cache for gre route info keyed by interface index. This stays tiny in practice.
    cached_gre_info: Vec<GreRouteInfo>,
}

struct RoutingTableDisplay<'a>(&'a [Route<Ipv4Addr>]);

impl fmt::Display for RoutingTableDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return f.write_str("<empty>");
        }

        for (i, route) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str("\n")?;
            }
            format_route(f, route)?;
        }

        Ok(())
    }
}

impl Router {
    pub fn new() -> Result<Self, io::Error> {
        Self::from_tables(RoutingTables::from_netlink(RouteTable::Main)?)
    }

    pub fn from_tables(tables: RoutingTables) -> Result<Self, io::Error> {
        let ipv4_lpm = Ipv4Lpm::build(tables.routes.as_slice());
        let mut router = Self {
            neighbors: tables.neighbors,
            routes: tables.routes,
            ipv4_lpm,
            interfaces: tables.interfaces,
            cached_default_route: None,
            cached_gre_info: Vec::new(),
        };

        let mut has_gre_interface = false;
        for interface in router.interfaces.iter() {
            if interface.gre_tunnel.is_some() {
                has_gre_interface = true;
                if let Some(gre) = router.interface_gre_route_info(interface) {
                    router.cached_gre_info.push(gre);
                }
            }
        }
        if router.cached_gre_info.is_empty() && has_gre_interface {
            warn!("GRE cache: GRE interface(s) present but none with valid remote resolved");
        }

        router.cached_default_route = match router.default_route() {
            Ok(hop) => Some(hop),
            Err(RouteError::NoRouteFound(_)) => None,
            Err(e) => return Err(io::Error::other(e)),
        };

        Ok(router)
    }

    pub fn routing_table(&self) -> impl fmt::Display + '_ {
        RoutingTableDisplay(self.routes.as_slice())
    }

    fn cached_gre_route_info(&self, if_index: u32) -> Option<&GreRouteInfo> {
        self.cached_gre_info
            .iter()
            .find(|gre| gre.if_index == if_index)
    }

    fn gre_route_info(&self, if_index: u32) -> Option<GreRouteInfo> {
        if let Some(gre) = self.cached_gre_route_info(if_index) {
            return Some(gre.clone());
        }

        let interface = self
            .interfaces
            .iter()
            .find(|interface| interface.if_index == if_index)?;
        self.interface_gre_route_info(interface)
    }

    fn resolve_next_hop(
        &self,
        route_ip: Ipv4Addr,
        route: &Route<Ipv4Addr>,
    ) -> Result<NextHop, RouteError> {
        let if_index = route
            .out_if_index
            .ok_or(RouteError::MissingOutputInterface)?;
        let next_hop_ip = IpAddr::V4(route.gateway.unwrap_or(route_ip));
        let preferred_src_ip = route.preferred_src;

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

        if let Some(gre) = self.gre_route_info(if_index) {
            return Ok(NextHop {
                if_index,
                ip_addr: next_hop_ip,
                mac_addr: Some(gre.mac_addr),
                preferred_src_ip,
                gre: Some(gre),
            });
        }

        let mac_addr = self.neighbors.lookup(next_hop_ip, if_index).cloned();
        Ok(NextHop {
            ip_addr: next_hop_ip,
            mac_addr,
            if_index,
            preferred_src_ip,
            gre: None,
        })
    }

    fn default_route(&self) -> Result<NextHop, RouteError> {
        let default_route = self
            .ipv4_lpm
            .default_route()
            .and_then(|route_idx| self.routes.get(route_idx as usize))
            .ok_or(RouteError::NoRouteFound(IpAddr::V4(Ipv4Addr::UNSPECIFIED)))?;
        self.resolve_next_hop(Ipv4Addr::UNSPECIFIED, default_route)
    }

    pub fn default(&self) -> Result<NextHop, RouteError> {
        if let Some(default_route) = &self.cached_default_route {
            Ok(default_route.clone())
        } else {
            self.default_route()
        }
    }

    fn lookup_route_v4(&self, dest_ip: Ipv4Addr) -> Option<&Route<Ipv4Addr>> {
        self.ipv4_lpm
            .lookup(dest_ip)
            .and_then(|route_idx| self.routes.get(route_idx as usize))
    }

    pub fn route_v4(&self, dest_ip: Ipv4Addr) -> Result<NextHop, RouteError> {
        let route = self
            .lookup_route_v4(dest_ip)
            .ok_or(RouteError::NoRouteFound(dest_ip.into()))?;
        self.resolve_next_hop(dest_ip, route)
    }

    fn interface_gre_route_info(&self, interface: &InterfaceInfo) -> Option<GreRouteInfo> {
        let tunnel_info = interface.gre_tunnel.as_ref()?;
        let remote = match tunnel_info.remote {
            IpAddr::V4(remote) => remote,
            IpAddr::V6(_) => return None,
        };
        let local = match tunnel_info.local {
            IpAddr::V4(local) => local,
            IpAddr::V6(_) => return None,
        };
        // Skip unconfigured tunnels (remote/local 0.0.0.0)
        if remote == Ipv4Addr::UNSPECIFIED || local == Ipv4Addr::UNSPECIFIED {
            return None;
        }

        let underlay_route = self.lookup_route_v4(remote)?;
        let underlay_if_index = underlay_route.out_if_index?;
        let underlay_interface = self
            .interfaces
            .iter()
            .find(|candidate| candidate.if_index == underlay_if_index)?;
        if underlay_interface.is_gre() {
            warn!(
                "GRE interface {} has remote {} that routes via another GRE interface {}. \
                 gre-over-gre is not supported.",
                interface.if_index, tunnel_info.remote, underlay_interface.if_index
            );
            return None;
        }
        let underlay_next_hop_ip = IpAddr::V4(underlay_route.gateway.unwrap_or(remote));
        let mac_addr = self
            .neighbors
            .lookup(underlay_next_hop_ip, underlay_if_index)
            .copied()?;

        Some(GreRouteInfo {
            if_index: interface.if_index,
            tunnel_info: tunnel_info.clone(),
            mac_addr,
        })
    }
}

fn format_route(f: &mut fmt::Formatter<'_>, route: &Route<Ipv4Addr>) -> fmt::Result {
    match route.destination {
        Some(destination) if route.dst_len == 32 => write!(f, "{destination}")?,
        Some(destination) => write!(f, "{destination}/{}", route.dst_len)?,
        None => f.write_str("default")?,
    }

    if let Some(gateway) = route.gateway {
        write!(f, " via {gateway}")?;
    }
    if let Some(if_index) = route.out_if_index {
        write!(f, " dev if{if_index}")?;
    }
    if let Some(preferred_src) = route.preferred_src {
        write!(f, " src {preferred_src}")?;
    }
    if let Some(priority) = route.priority {
        write!(f, " metric {priority}")?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::netlink::{MacAddress, NeighborEntry, RouteEntry},
        libc::{AF_INET, NUD_REACHABLE},
        std::net::{IpAddr, Ipv4Addr},
    };

    fn test_route(destination: Option<Ipv4Addr>, out_if_index: u32) -> Route<Ipv4Addr> {
        Route {
            destination,
            gateway: None,
            preferred_src: None,
            out_if_index: Some(out_if_index),
            priority: None,
            type_: 0,
            dst_len: 32,
        }
    }

    fn test_route_entry(
        destination: Option<Ipv4Addr>,
        gateway: Option<Ipv4Addr>,
        out_if_index: u32,
        dst_len: u8,
        table: u32,
    ) -> RouteEntry {
        test_route_entry_with_priority(destination, gateway, out_if_index, dst_len, table, None)
    }

    fn test_route_entry_with_priority(
        destination: Option<Ipv4Addr>,
        gateway: Option<Ipv4Addr>,
        out_if_index: u32,
        dst_len: u8,
        table: u32,
        priority: Option<u32>,
    ) -> RouteEntry {
        RouteEntry {
            destination: destination.map(IpAddr::V4),
            gateway: gateway.map(IpAddr::V4),
            pref_src: None,
            out_if_index: Some(out_if_index as i32),
            in_if_index: None,
            priority,
            table: Some(table),
            protocol: 0,
            scope: 0,
            type_: 0,
            family: AF_INET as u8,
            dst_len,
            flags: 0,
        }
    }

    fn router_from_tables(
        neighbors: Vec<NeighborEntry>,
        routes: Vec<Route<Ipv4Addr>>,
        interfaces: Vec<InterfaceInfo>,
    ) -> Router {
        let tables = RoutingTables::new(
            Routes::new(RouteTable::Main, routes),
            Neighbors::new(neighbors),
            Interfaces::new(interfaces),
        );
        Router::from_tables(tables).unwrap()
    }

    fn empty_tables() -> RoutingTables {
        RoutingTables::new(
            Routes::new(RouteTable::Main, Vec::new()),
            Neighbors::new(Vec::new()),
            Interfaces::new(Vec::new()),
        )
    }

    fn dual_default_tables(primary_gateway: Ipv4Addr, backup_gateway: Ipv4Addr) -> RoutingTables {
        let mut tables = empty_tables();
        assert!(tables.upsert_neighbor(NeighborEntry {
            destination: Some(IpAddr::V4(primary_gateway)),
            lladdr: Some(MacAddress([0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0x01])),
            ifindex: 1,
            state: NUD_REACHABLE,
        }));
        assert!(tables.upsert_neighbor(NeighborEntry {
            destination: Some(IpAddr::V4(backup_gateway)),
            lladdr: Some(MacAddress([0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0x02])),
            ifindex: 2,
            state: NUD_REACHABLE,
        }));
        assert!(tables.upsert_route(test_route_entry_with_priority(
            None,
            Some(backup_gateway),
            2,
            0,
            u32::from(RouteTable::Main),
            Some(200),
        )));
        assert!(tables.upsert_route(test_route_entry_with_priority(
            None,
            Some(primary_gateway),
            1,
            0,
            u32::from(RouteTable::Main),
            Some(100),
        )));
        tables
    }

    #[test]
    fn test_router() {
        let mut tables = empty_tables();
        let before_routes_len = tables.routes.iter().len();

        let gateway = Ipv4Addr::new(10, 255, 255, 1);
        let neighbor = NeighborEntry {
            destination: Some(IpAddr::V4(gateway)),
            lladdr: Some(MacAddress([0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0x01])),
            ifindex: 1,
            state: NUD_REACHABLE,
        };
        assert!(tables.upsert_neighbor(neighbor));

        // Create a unique, private IPv4 /32 route to avoid collisions
        let test_dst = Ipv4Addr::new(10, 255, 255, 123);
        let route = RouteEntry {
            destination: Some(IpAddr::V4(test_dst)),
            gateway: Some(IpAddr::V4(gateway)),
            pref_src: None,
            out_if_index: Some(1),
            in_if_index: None,
            priority: None,
            table: Some(u32::from(RouteTable::Main)),
            protocol: 0,
            scope: 0,
            type_: 0,
            family: AF_INET as u8,
            dst_len: 32,
            flags: 0,
        };

        // Upsert new route and check that it was inserted and routes are dirty
        assert!(tables.upsert_route(route.clone()));
        assert!(tables.routes.iter().any(|r| {
            r.destination == Some(test_dst)
                && r.gateway == Some(gateway)
                && r.out_if_index == Some(1)
        }));
        assert!(tables.routes.iter().len() >= before_routes_len);

        let router = Router::from_tables(tables.clone()).unwrap();
        let next_hop = router.route_v4(test_dst).unwrap();
        assert_eq!(next_hop.if_index, 1);
        assert_eq!(next_hop.ip_addr, IpAddr::V4(gateway));

        // Delete using same key should remove the route
        assert!(tables.remove_route(route.clone()));
        assert!(
            tables
                .routes
                .iter()
                .all(|r| r.destination != Some(test_dst))
        );
        assert_eq!(tables.routes.iter().len(), before_routes_len);
    }

    #[test]
    fn test_routing_table_display() {
        let router = router_from_tables(
            vec![],
            vec![
                Route {
                    destination: Some(Ipv4Addr::new(10, 0, 0, 0)),
                    gateway: Some(Ipv4Addr::new(192, 168, 1, 1)),
                    preferred_src: Some(Ipv4Addr::new(192, 168, 1, 10)),
                    out_if_index: Some(2),
                    priority: Some(100),
                    type_: 0,
                    dst_len: 24,
                },
                Route {
                    destination: None,
                    gateway: Some(Ipv4Addr::new(192, 168, 1, 254)),
                    preferred_src: None,
                    out_if_index: Some(3),
                    priority: None,
                    type_: 0,
                    dst_len: 0,
                },
            ],
            vec![],
        );

        assert_eq!(
            router.routing_table().to_string(),
            "10.0.0.0/24 via 192.168.1.1 dev if2 src 192.168.1.10 metric 100\ndefault via \
             192.168.1.254 dev if3"
        );
    }

    #[test]
    fn test_neighbors_table() {
        let mut tables = empty_tables();
        let before_neigh_len = tables.neighbors.iter().len();

        // Create a unique, private neighbor entry on a dummy ifindex
        let neigh_ip = Ipv4Addr::new(10, 255, 255, 77);
        let entry = NeighborEntry {
            destination: Some(IpAddr::V4(neigh_ip)),
            lladdr: Some(MacAddress([0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0x01])),
            ifindex: 1,
            state: NUD_REACHABLE,
        };

        // Upsert new neighbor and check that it was inserted and neighbors are dirty
        assert!(tables.upsert_neighbor(entry.clone()));
        assert!(tables.neighbors.iter().any(|n| n == &entry));
        assert!(tables.neighbors.iter().len() >= before_neigh_len);

        // Delete neighbor and check that it was deleted
        assert!(tables.remove_neighbor(neigh_ip, 1));
        assert!(tables.neighbors.iter().all(|n| n != &entry));
        assert_eq!(tables.neighbors.iter().len(), before_neigh_len);
    }

    #[test]
    fn test_interface_table() {
        let mut tables = empty_tables();
        let before_interface_len = tables.interfaces.iter().len();

        // Create a unique, private interface with a dummy ifindex
        let test_if_index = 99999;
        let interface = InterfaceInfo {
            if_index: test_if_index,
            gre_tunnel: None,
        };

        // Upsert new interface and check that it was inserted
        assert!(tables.upsert_interface(interface.clone()));
        assert!(tables.interfaces.iter().any(|i| i == &interface));
        assert!(tables.interfaces.iter().len() >= before_interface_len);

        // Upsert same interface with no changes should return false
        assert!(!tables.upsert_interface(interface.clone()));

        // Upsert with changes should return true
        let mut modified_interface = interface.clone();
        modified_interface.gre_tunnel = Some(GreTunnelInfo {
            local: IpAddr::V4(Ipv4Addr::new(10, 255, 255, 2)),
            remote: IpAddr::V4(Ipv4Addr::new(10, 255, 255, 1)),
            ttl: 0,
            tos: 0,
            pmtudisc: 0,
        });
        assert!(tables.upsert_interface(modified_interface.clone()));
        assert!(tables.interfaces.iter().any(|i| i == &modified_interface));
        assert!(tables.interfaces.iter().all(|i| i != &interface));

        // Delete interface and check that it was deleted
        assert!(tables.remove_interface(test_if_index));
        assert!(
            tables
                .interfaces
                .iter()
                .all(|i| i.if_index != test_if_index)
        );
        assert_eq!(tables.interfaces.iter().len(), before_interface_len);
    }

    #[test]
    fn test_routes_ignore_other_table_updates() {
        let test_dst = Ipv4Addr::new(10, 0, 0, 1);
        let main_gateway = Ipv4Addr::new(10, 0, 0, 2);
        let other_gateway = Ipv4Addr::new(10, 0, 0, 3);
        let main_table = u32::from(RouteTable::Main);

        let mut tables = empty_tables();
        assert!(tables.upsert_neighbor(NeighborEntry {
            destination: Some(IpAddr::V4(main_gateway)),
            lladdr: Some(MacAddress([0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0x01])),
            ifindex: 1,
            state: NUD_REACHABLE,
        }));
        assert!(tables.upsert_route(test_route_entry(
            Some(test_dst),
            Some(main_gateway),
            1,
            32,
            main_table,
        )));

        assert!(!tables.upsert_route(test_route_entry(
            Some(test_dst),
            Some(other_gateway),
            2,
            32,
            100,
        )));

        let router = Router::from_tables(tables).unwrap();
        let next_hop = router.route_v4(test_dst).unwrap();
        assert_eq!(next_hop.if_index, 1);
        assert_eq!(next_hop.ip_addr, IpAddr::V4(main_gateway));
    }

    #[test]
    fn test_routes_ignore_other_table_deletes() {
        let test_dst = Ipv4Addr::new(10, 0, 0, 1);
        let main_gateway = Ipv4Addr::new(10, 0, 0, 2);
        let main_table = u32::from(RouteTable::Main);

        let mut tables = empty_tables();
        assert!(tables.upsert_neighbor(NeighborEntry {
            destination: Some(IpAddr::V4(main_gateway)),
            lladdr: Some(MacAddress([0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0x01])),
            ifindex: 1,
            state: NUD_REACHABLE,
        }));
        assert!(tables.upsert_route(test_route_entry(
            Some(test_dst),
            Some(main_gateway),
            1,
            32,
            main_table,
        )));

        assert!(!tables.remove_route(test_route_entry(
            Some(test_dst),
            Some(Ipv4Addr::new(10, 0, 0, 3)),
            2,
            32,
            100,
        )));

        let router = Router::from_tables(tables).unwrap();
        let next_hop = router.route_v4(test_dst).unwrap();
        assert_eq!(next_hop.if_index, 1);
        assert_eq!(next_hop.ip_addr, IpAddr::V4(main_gateway));
    }

    #[test]
    fn test_multiple_gre_interfaces() {
        let remote1 = Ipv4Addr::new(10, 0, 0, 1);
        let remote2 = Ipv4Addr::new(10, 0, 0, 2);
        let gre_dest1 = Ipv4Addr::new(192, 168, 0, 1);
        let gre_dest2 = Ipv4Addr::new(192, 168, 0, 2);
        let if_index_underlay = 1;
        let if_index_gre1 = 100;
        let if_index_gre2 = 200;
        let mac1 = MacAddress([0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0x01]);
        let mac2 = MacAddress([0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0x02]);

        let neighbors = vec![
            NeighborEntry {
                destination: Some(IpAddr::V4(remote1)),
                lladdr: Some(mac1),
                ifindex: if_index_underlay,
                state: NUD_REACHABLE,
            },
            NeighborEntry {
                destination: Some(IpAddr::V4(remote2)),
                lladdr: Some(mac2),
                ifindex: if_index_underlay,
                state: NUD_REACHABLE,
            },
        ];

        let routes = vec![
            test_route(Some(remote1), if_index_underlay as u32),
            test_route(Some(remote2), if_index_underlay as u32),
            test_route(Some(gre_dest1), if_index_gre1 as u32),
            test_route(Some(gre_dest2), if_index_gre2 as u32),
        ];

        let interfaces = vec![
            InterfaceInfo {
                if_index: if_index_underlay as u32,
                gre_tunnel: None,
            },
            InterfaceInfo {
                if_index: if_index_gre1 as u32,
                gre_tunnel: Some(GreTunnelInfo {
                    local: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 3)),
                    remote: IpAddr::V4(remote1),
                    ttl: 0,
                    tos: 0,
                    pmtudisc: 0,
                }),
            },
            InterfaceInfo {
                if_index: if_index_gre2 as u32,
                gre_tunnel: Some(GreTunnelInfo {
                    local: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 4)),
                    remote: IpAddr::V4(remote2),
                    ttl: 0,
                    tos: 0,
                    pmtudisc: 0,
                }),
            },
        ];

        let router = router_from_tables(neighbors, routes, interfaces);
        let hop1 = router.route_v4(gre_dest1).unwrap();
        assert_eq!(hop1.if_index, if_index_gre1 as u32);
        assert_eq!(hop1.mac_addr, Some(mac1));
        assert_eq!(
            hop1.gre.as_ref().map(|gre| gre.if_index),
            Some(if_index_gre1 as u32)
        );

        let hop2 = router.route_v4(gre_dest2).unwrap();
        assert_eq!(hop2.if_index, if_index_gre2 as u32);
        assert_eq!(hop2.mac_addr, Some(mac2));
        assert_eq!(
            hop2.gre.as_ref().map(|gre| gre.if_index),
            Some(if_index_gre2 as u32)
        );
    }

    #[test]
    fn test_default_route_via_gre() {
        let remote = Ipv4Addr::new(10, 0, 0, 1);
        let dest = Ipv4Addr::new(203, 0, 113, 9);
        let if_index_underlay = 1;
        let if_index_gre = 100;
        let mac = MacAddress([0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0x01]);

        let neighbors = vec![NeighborEntry {
            destination: Some(IpAddr::V4(remote)),
            lladdr: Some(mac),
            ifindex: if_index_underlay,
            state: NUD_REACHABLE,
        }];

        let routes = vec![
            test_route(Some(remote), if_index_underlay as u32),
            Route {
                destination: None,
                gateway: None,
                preferred_src: None,
                out_if_index: Some(if_index_gre as u32),
                priority: None,
                type_: 0,
                dst_len: 0,
            },
        ];

        let interfaces = vec![
            InterfaceInfo {
                if_index: if_index_underlay as u32,
                gre_tunnel: None,
            },
            InterfaceInfo {
                if_index: if_index_gre as u32,
                gre_tunnel: Some(GreTunnelInfo {
                    local: IpAddr::V4(Ipv4Addr::new(10, 1, 0, 1)),
                    remote: IpAddr::V4(remote),
                    ttl: 0,
                    tos: 0,
                    pmtudisc: 0,
                }),
            },
        ];

        let router = router_from_tables(neighbors, routes, interfaces);

        let hop_default = router.default().unwrap();
        assert_eq!(hop_default.if_index, if_index_gre as u32);
        assert_eq!(hop_default.mac_addr, Some(mac));
        assert_eq!(
            hop_default.gre.as_ref().map(|gre| gre.if_index),
            Some(if_index_gre as u32)
        );

        let hop_route = router.route_v4(dest).unwrap();
        assert_eq!(hop_route.if_index, if_index_gre as u32);
        assert_eq!(hop_route.mac_addr, Some(mac));
        assert_eq!(
            hop_route.gre.as_ref().map(|gre| gre.if_index),
            Some(if_index_gre as u32)
        );
    }

    #[test]
    fn test_missing_output_interface_blocks_fallback_route() {
        let blocked_dst = Ipv4Addr::new(10, 0, 0, 2);
        let default_gateway = Ipv4Addr::new(10, 0, 0, 1);

        let mut tables = empty_tables();
        assert!(tables.upsert_neighbor(NeighborEntry {
            destination: Some(IpAddr::V4(default_gateway)),
            lladdr: Some(MacAddress([0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0x01])),
            ifindex: 1,
            state: NUD_REACHABLE,
        }));

        assert!(tables.upsert_route(test_route_entry(
            None,
            Some(default_gateway),
            1,
            0,
            u32::from(RouteTable::Main),
        )));

        assert!(tables.upsert_route(RouteEntry {
            destination: Some(IpAddr::V4(blocked_dst)),
            gateway: None,
            pref_src: None,
            out_if_index: None, // missing output interface should block this route
            in_if_index: None,
            priority: None,
            table: Some(u32::from(RouteTable::Main)),
            protocol: 0,
            scope: 0,
            type_: 0,
            family: AF_INET as u8,
            dst_len: 32,
            flags: 0,
        }));

        let router = Router::from_tables(tables).unwrap();
        assert!(matches!(
            router.route_v4(blocked_dst),
            Err(RouteError::MissingOutputInterface)
        ));
    }

    #[test]
    fn test_same_prefix_diff_priority() {
        let primary = Ipv4Addr::new(10, 0, 0, 1);
        let backup = Ipv4Addr::new(10, 0, 0, 2);
        let tables = dual_default_tables(primary, backup);

        let router = Router::from_tables(tables.clone()).unwrap();
        let next_hop = router.default().unwrap();
        assert_eq!(next_hop.if_index, 1);
        assert_eq!(next_hop.ip_addr, IpAddr::V4(primary));

        {
            let mut tables = tables.clone();
            assert!(tables.remove_route(test_route_entry_with_priority(
                None,
                Some(backup),
                2,
                0,
                u32::from(RouteTable::Main),
                Some(200),
            )));

            let router = Router::from_tables(tables).unwrap();
            let next_hop = router.default().unwrap();
            assert_eq!(next_hop.if_index, 1);
            assert_eq!(next_hop.ip_addr, IpAddr::V4(primary));
        }

        let mut tables = tables;
        assert!(tables.remove_route(test_route_entry_with_priority(
            None,
            Some(primary),
            1,
            0,
            u32::from(RouteTable::Main),
            Some(100),
        )));

        let router = Router::from_tables(tables).unwrap();
        let next_hop = router.default().unwrap();
        assert_eq!(next_hop.if_index, 2);
        assert_eq!(next_hop.ip_addr, IpAddr::V4(backup));
    }
}
