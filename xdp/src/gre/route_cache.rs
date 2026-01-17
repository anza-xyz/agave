#![allow(clippy::arithmetic_side_effects)]

use {
    crate::{
        netlink::{InterfaceInfo, MacAddress},
        route::NextHop,
    },
    std::{
        net::{IpAddr, Ipv4Addr},
        sync::Arc,
    },
};

/// Cache for resolving the outer GRE destination MAC address.
#[derive(Default)]
pub struct GreRouteCache {
    cached_remote: Option<(Ipv4Addr, MacAddress)>,
}

impl GreRouteCache {
    pub fn new() -> Self {
        Self {
            cached_remote: None,
        }
    }

    /// Resolve outer destination MAC for GRE packets, with a single-entry cache.
    pub fn resolve_outer_dst_mac<R>(
        &mut self,
        gre_remote: Ipv4Addr,
        route_fn: &R,
    ) -> Option<MacAddress>
    where
        R: Fn(&IpAddr) -> Option<(NextHop, Arc<InterfaceInfo>)>,
    {
        if let Some((cached_remote, cached_mac)) = self.cached_remote.as_ref() {
            if *cached_remote == gre_remote {
                return Some(*cached_mac);
            }
        }

        let (next_hop, _iface) = route_fn(&IpAddr::V4(gre_remote))?;
        let mac = next_hop.mac_addr?;

        self.cached_remote = Some((gre_remote, mac));
        Some(mac)
    }
}
