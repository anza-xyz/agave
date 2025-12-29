#![allow(clippy::arithmetic_side_effects)]

use {
    crate::{
        gre::packet::{construct_gre_packet, GreConfig, PacketError, INNER_PACKET_HEADER_SIZE, GRE_HEADER_BASE_SIZE, GRE_KEY_SIZE},
        netlink::{GreTunnelInfo, InterfaceInfo, MacAddress},
        packet::{ETH_HEADER_SIZE, IP_HEADER_SIZE},
        route::NextHop,
    },
    std::net::{IpAddr, Ipv4Addr, SocketAddr},
    thiserror::Error,
};

/// Manages GRE encapsulation state (cached MAC addresses) and provides
/// methods for encapsulating packets with GRE headers.
#[derive(Default)]
pub struct GreEncapsulator {
    /// Cache for GRE remote MAC address lookup
    /// Assumes a single GRE tunnel per host
    cached_remote: Option<(Ipv4Addr, MacAddress)>,
}

impl GreEncapsulator {
    pub fn is_gre(interface_info: &InterfaceInfo) -> bool {
        interface_info.gre_tunnel.is_some()
    }

    /// Extract GRE tunnel endpoints from interface info
    pub fn extract_gre_endpoints(gre: &GreTunnelInfo) -> Option<(Ipv4Addr, Ipv4Addr)> {
        match (gre.local, gre.remote) {
            (Some(IpAddr::V4(local)), Some(IpAddr::V4(remote))) => Some((local, remote)),
            _ => None,
        }
    }

    /// Get outer destination MAC for the GRE packet (cached for performance)
    pub fn get_outer_dst_mac<R>(&mut self, gre_remote: Ipv4Addr, route_fn: &R) -> Option<MacAddress>
    where
        R: Fn(&IpAddr) -> Option<(NextHop, InterfaceInfo)>,
    {
        if let Some((cached_remote, cached_mac)) = self.cached_remote.as_ref() {
            if *cached_remote == gre_remote {
                return Some(*cached_mac);
            }
        }

        // Cache miss - must lookup route to GRE remote
        let (nh, _iface) = route_fn(&IpAddr::V4(gre_remote))?;
        let mac = nh.mac_addr?;

        // update cache
        self.cached_remote = Some((gre_remote, mac));
        Some(mac)
    }

    /// Invalidate GRE remote cache. //greg: todo not sure this is a good idea anymore
    pub fn invalidate_cache(&mut self) {
        self.cached_remote = None;
    }

    /// Calculate size of a GRE-encapsulated packet
    pub fn calculate_packet_size(payload_len: usize, config: &GreConfig) -> usize {
        let gre_header_size = if config.okey.is_some() {
            GRE_HEADER_BASE_SIZE + GRE_KEY_SIZE
        } else {
            GRE_HEADER_BASE_SIZE
        };
        (ETH_HEADER_SIZE + IP_HEADER_SIZE + gre_header_size + INNER_PACKET_HEADER_SIZE).saturating_add(payload_len)
    }

    /// Encapsulate a packet with GRE
    /// Returns the constructed packet length on success, or an error if encapsulation fails.
    #[allow(clippy::too_many_arguments)]
    pub fn encapsulate_packet<R>(
        &mut self,
        packet: &mut [u8],
        payload: &[u8],
        dst_addr: SocketAddr,
        src_ip: Ipv4Addr,
        src_port: u16,
        src_mac: MacAddress,
        next_hop: &NextHop,
        gre_config: &GreConfig,
        route_fn: &R,
    ) -> Result<usize, EncapsulationError>
    where
        R: Fn(&IpAddr) -> Option<(NextHop, InterfaceInfo)>,
    {
        // Get outer destination MAC (cached for performance)
        let outer_dst_mac = self
            .get_outer_dst_mac(gre_config.remote, route_fn)
            .ok_or(EncapsulationError::RouteLookupFailed)?;

        let inner_src_ip = next_hop.preferred_src_ip.unwrap_or(src_ip);
        let dst_ip = match dst_addr.ip() {
            IpAddr::V4(ip) => ip,
            IpAddr::V6(_) => return Err(EncapsulationError::Ipv6NotSupported),
        };

        let gre_packet_len = construct_gre_packet(
            packet,
            &inner_src_ip,
            &dst_ip,
            src_port,
            dst_addr.port(),
            payload,
            &src_mac.0,
            &outer_dst_mac.0,
            gre_config,
        )?;

        Ok(gre_packet_len)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum EncapsulationError {
    #[error("invalid GRE tunnel endpoints")]
    InvalidEndpoints,
    #[error("failed to resolve route to GRE remote")]
    RouteLookupFailed,
    #[error("IPv6 not supported")]
    Ipv6NotSupported,
    #[error("packet construction failed: {0}")]
    PacketConstruction(#[from] PacketError),
}
