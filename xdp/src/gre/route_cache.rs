#![allow(clippy::arithmetic_side_effects)]

use {
    crate::{
        netlink::{InterfaceInfo, MacAddress},
        route::NextHop,
    },
    std::net::{IpAddr, Ipv4Addr},
};

/// Cache for resolving the outer GRE destination MAC address.
#[derive(Default)]
pub struct GreRouteCache {
    cached_remote: Option<CachedGreRoute>,
}

struct CachedGreRoute {
    remote: Ipv4Addr,
    mac: MacAddress,
    route_version: u64,
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
        route_version: u64,
        route_fn: &R,
    ) -> Option<MacAddress>
    where
        R: Fn(&IpAddr) -> Option<(NextHop, u64)>,
    {
        if let Some(cached) = self.cached_remote.as_ref() {
            if cached.remote == gre_remote && cached.route_version == route_version {
                return Some(cached.mac);
            }
        }

        let (next_hop, route_version) = route_fn(&IpAddr::V4(gre_remote))?;
        let mac = next_hop.mac_addr?;

        self.cached_remote = Some(CachedGreRoute {
            remote: gre_remote,
            mac,
            route_version,
        });
        Some(mac)
    }
}

/// Cache for InterfaceInfo lookups
#[derive(Default)]
pub struct InterfaceInfoCache {
    cached_interfaces: [Option<(u32, InterfaceInfo)>; 2],
}

impl InterfaceInfoCache {
    pub fn new() -> Self {
        Self {
            cached_interfaces: [None, None],
        }
    }

    // For normal operation, all outgoing packets should go out the same interface
    // When running w/ GRE support, we need to cache a second interface to support
    // packets that go out the gre tunnel interface
    pub fn get<I>(&mut self, if_index: u32, interface_fn: &I) -> Option<&InterfaceInfo>
    where
        I: Fn(u32) -> Option<InterfaceInfo>,
    {
        let first_index = self.cached_interfaces[0]
            .as_ref()
            .map(|(cached_index, _)| *cached_index);
        if first_index == Some(if_index) {
            return self.cached_interfaces[0]
                .as_ref()
                .map(|(_, cached_info)| cached_info);
        }

        let second_index = self.cached_interfaces[1]
            .as_ref()
            .map(|(cached_index, _)| *cached_index);
        if second_index == Some(if_index) {
            return self.cached_interfaces[1]
                .as_ref()
                .map(|(_, cached_info)| cached_info);
        }

        let info = interface_fn(if_index)?;
        if let Some(empty_index) = self
            .cached_interfaces
            .iter()
            .position(|slot| slot.is_none())
        {
            self.cached_interfaces[empty_index] = Some((if_index, info));
            return self.cached_interfaces[empty_index]
                .as_ref()
                .map(|(_, cached_info)| cached_info);
        }

        // If no empty slot is found, replace the first slot
        self.cached_interfaces[0] = Some((if_index, info));
        self.cached_interfaces[0]
            .as_ref()
            .map(|(_, cached_info)| cached_info)
    }
}
