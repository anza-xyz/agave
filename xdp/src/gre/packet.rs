#![allow(clippy::arithmetic_side_effects)]

use {
    crate::packet::{
        write_eth_header, write_ip_header, write_ip_header_for_udp, write_udp_header,
        ETH_HEADER_SIZE, IP_HEADER_SIZE, UDP_HEADER_SIZE,
    },
    libc::{ETH_P_IP, IPPROTO_GRE},
    std::net::Ipv4Addr,
    thiserror::Error,
};

pub const INNER_PACKET_HEADER_SIZE: usize = IP_HEADER_SIZE + UDP_HEADER_SIZE;
/// Minimal GRE header size in bytes without optional fields
pub const GRE_HEADER_BASE_SIZE: usize = 4;
const GRE_HEADER_FLAGS_VERSION_BASIC: u16 = 0x0000;

/// Calculate total packet size for GRE encapsulation.
pub const fn gre_packet_size(payload_len: usize) -> usize {
    (ETH_HEADER_SIZE + IP_HEADER_SIZE + GRE_HEADER_BASE_SIZE + INNER_PACKET_HEADER_SIZE)
        .saturating_add(payload_len)
}

/// GRE tunnel configuration for packet construction
///
/// Note: Only supports basic GRE header (no optional fields).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelInfo {
    /// Source IP address for the GRE tunnel header
    pub local: Ipv4Addr,
    /// Destination IP address for the GRE tunnel header
    pub remote: Ipv4Addr,
    pub ttl: Option<u8>,
    pub tos: Option<u8>,
    /// PMTU discovery setting (IFLA_GRE_PMTUDISC)
    pub pmtudisc: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PacketError {
    #[error("packet buffer too small: need {needed} bytes, have {have} bytes")]
    BufferTooSmall { needed: usize, have: usize },
}

/// GRE header structure
///
/// Currently only supports basic GRE header format:
/// - flags_version: Always 0x0000
/// - protocol: Always 0x0800 (IPv4)
///
/// Optional fields (C, K, S flags) are not yet supported but may be added in the future.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct GreHeader {
    pub flags_version: u16,
    pub protocol: u16,
    // Note: Optional fields (key, checksum, sequence) will be added here
    // when full GRE support is implemented. Keeping struct simple for now.
}

impl GreHeader {
    pub fn new(protocol_type: u16) -> Self {
        Self {
            flags_version: GRE_HEADER_FLAGS_VERSION_BASIC,
            protocol: protocol_type,
        }
    }

    /// Write the GRE header to a packet buffer
    pub fn write_to_packet(&self, packet: &mut [u8]) {
        packet[0..2].copy_from_slice(&self.flags_version.to_be_bytes());
        packet[2..4].copy_from_slice(&self.protocol.to_be_bytes());
    }
}

impl TunnelInfo {
    pub fn new(local: Ipv4Addr, remote: Ipv4Addr) -> Self {
        Self {
            local,
            remote,
            ttl: None,
            tos: None,
            pmtudisc: None,
        }
    }
}

/// Conversion from netlink's GreTunnelInfo to GreConfig
///
/// This allows GRE packet construction to work with tunnel info discovered via netlink,
/// while keeping the core GRE module independent of netlink.
impl TryFrom<&crate::netlink::GreTunnelInfo> for TunnelInfo {
    type Error = ();

    fn try_from(info: &crate::netlink::GreTunnelInfo) -> Result<Self, Self::Error> {
        let Some(std::net::IpAddr::V4(local)) = info.local else {
            return Err(());
        };
        let Some(std::net::IpAddr::V4(remote)) = info.remote else {
            return Err(());
        };

        Ok(Self {
            local,
            remote,
            ttl: info.ttl,
            tos: info.tos,
            pmtudisc: info.pmtudisc,
        })
    }
}

/// Wrap a UDP packet with L3 GRE encapsulation
///
/// Takes the original packet (IP + UDP + payload) and wraps it
/// with Ethernet + IP + GRE headers, to create:
/// [Ethernet] [Outer IP] [GRE] [Inner IP] [UDP] [Payload]
fn wrap_packet_with_gre(
    packet: &mut [u8],
    gre_src_mac: &[u8; 6],
    gre_dst_mac: &[u8; 6],
    inner_packet_len: usize,
    gre_header: &GreHeader,
    config: &TunnelInfo,
) {
    write_eth_header(packet, gre_src_mac, gre_dst_mac);

    let dont_fragment = config.pmtudisc.map(|v| v != 0).unwrap_or(true);

    // Write outer IP header (protocol = GRE = 47)
    let gre_payload_len = GRE_HEADER_BASE_SIZE + inner_packet_len;
    write_ip_header(
        &mut packet[ETH_HEADER_SIZE..],
        &config.local,
        &config.remote,
        gre_payload_len as u16,
        IPPROTO_GRE as u8,
        dont_fragment,
        config.ttl,
        config.tos,
    );

    // Write GRE header
    gre_header.write_to_packet(&mut packet[ETH_HEADER_SIZE + IP_HEADER_SIZE..]);
}

/// Construct an L3 GRE packet from a UDP payload
///
/// This function takes a UDP payload and constructs a complete L3 GRE packet
/// with the structure: [Ethernet] [Outer IP] [GRE] [Inner IP] [UDP] [Payload]
///
/// Returns the total length of the constructed GRE packet, or an error if the buffer is too small
#[allow(clippy::too_many_arguments)]
pub fn construct_gre_packet(
    packet: &mut [u8],
    src_ip: &Ipv4Addr,
    dst_ip: &Ipv4Addr,
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
    src_mac: &[u8; 6],
    dst_mac: &[u8; 6],
    config: &TunnelInfo,
) -> Result<(), PacketError> {
    let payload_len = payload.len();

    let gre_header = GreHeader::new(ETH_P_IP as u16);
    let inner_packet_len = INNER_PACKET_HEADER_SIZE.saturating_add(payload_len);
    let gre_packet_size =
        (ETH_HEADER_SIZE + IP_HEADER_SIZE + GRE_HEADER_BASE_SIZE).saturating_add(inner_packet_len);

    // Ensure packet buffer is large enough
    if packet.len() < gre_packet_size {
        return Err(PacketError::BufferTooSmall {
            needed: gre_packet_size,
            have: packet.len(),
        });
    }

    let inner_start = ETH_HEADER_SIZE + IP_HEADER_SIZE + GRE_HEADER_BASE_SIZE;

    write_ip_header_for_udp(
        &mut packet[inner_start..inner_start + IP_HEADER_SIZE],
        src_ip,
        dst_ip,
        (UDP_HEADER_SIZE + payload_len) as u16,
    );

    write_udp_header(
        &mut packet[inner_start + IP_HEADER_SIZE..inner_start + INNER_PACKET_HEADER_SIZE],
        src_ip,
        src_port,
        dst_ip,
        dst_port,
        payload_len as u16,
        false, // no checksums
    );

    // Write payload
    packet[inner_start + INNER_PACKET_HEADER_SIZE
        ..inner_start + INNER_PACKET_HEADER_SIZE + payload_len]
        .copy_from_slice(payload);

    wrap_packet_with_gre(
        packet,
        src_mac,
        dst_mac,
        inner_packet_len,
        &gre_header,
        config,
    );
    Ok(())
}
