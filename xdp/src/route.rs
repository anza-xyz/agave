use {
    crate::netlink::{
        netlink_get_neighbors, netlink_get_routes, MacAddress, NeighborEntry, RouteEntry,
    },
    libc::{AF_INET, AF_INET6},
    std::{
        io,
        net::{IpAddr, Ipv4Addr, Ipv6Addr},
        time::Instant,
    },
    thiserror::Error,
    tokio::sync::broadcast::{self, Receiver, Sender},
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

#[derive(Clone)]
pub enum RouteUpdate {
    RoutesChanged(Vec<RouteEntry>),
    ArpChanged(Vec<NeighborEntry>),
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

pub struct Router {
    routes: Vec<RouteEntry>,
    arp_table: ArpTable,
    last_update: Instant,
    update_receiver: Receiver<RouteUpdate>,
}

impl Router {
    pub fn new() -> Result<(Self, Sender<RouteUpdate>), io::Error> {
        let (update_sender, _) = broadcast::channel(10); // todo: maybe update

        let routes = netlink_get_routes(AF_INET as u8)?;
        let arp_table = ArpTable::new()?;

        Ok((
            Self {
                routes,
                arp_table,
                last_update: Instant::now(),
                update_receiver: update_sender.subscribe(),
            },
            update_sender,
        ))
    }

    pub fn subscribe(sender: &broadcast::Sender<RouteUpdate>) -> Result<Self, io::Error> {
        let routes = netlink_get_routes(AF_INET as u8)?;
        let arp_table = ArpTable::new()?;

        Ok(Self {
            routes,
            arp_table,
            last_update: Instant::now(),
            update_receiver: sender.subscribe(),
        })
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
    // replaces Router::route()??
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

    // Check for updates and apply them (non-blocking)
    pub fn try_update(&mut self) -> bool {
        let mut updated = false;
        let mut latest_routes = None;
        let mut latest_arp = None;

        // Drain all pending updates and keep only the latest
        // we are writing the whole routing table as updates each time, so we only need to keep the latest
        while let Ok(update) = self.update_receiver.try_recv() {
            match update {
                RouteUpdate::RoutesChanged(new_routes) => {
                    latest_routes = Some(new_routes);
                    updated = true;
                }
                RouteUpdate::ArpChanged(new_neighbors) => {
                    latest_arp = Some(new_neighbors);
                    updated = true;
                }
            }
        }

        // Apply only the latest updates
        if let Some(routes) = latest_routes {
            self.routes = routes;
            self.last_update = Instant::now();
            log::info!("greg: Updated routes: {} entries", self.routes.len());
        }

        if let Some(neighbors) = latest_arp {
            self.arp_table = ArpTable::from_neighbors(neighbors);
            self.last_update = Instant::now();
            log::info!(
                "greg: Updated ARP table: {} entries",
                self.arp_table.neighbors.len()
            );
        }

        updated
    }
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
        let (router, _) = Router::new().unwrap();
        let next_hop = router.route("1.1.1.1".parse().unwrap()).unwrap();
        eprintln!("{next_hop:?}");
    }
}
