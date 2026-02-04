#![allow(clippy::arithmetic_side_effects)]

use {
    crate::{
        netlink::{GreTunnelInfo, MacAddress},
        packet::{
            write_eth_header, write_ip_header, write_ip_header_for_udp, write_udp_header,
            ETH_HEADER_SIZE, IP_HEADER_SIZE, UDP_HEADER_SIZE,
        },
    },
    libc::{ETH_P_IP, IPPROTO_GRE},
    std::net::{IpAddr, Ipv4Addr},
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PacketError {
    #[error("packet buffer too small: need {needed} bytes, have {have} bytes")]
    BufferTooSmall { needed: usize, have: usize },
    #[error("invalid GRE tunnel endpoints")]
    InvalidTunnelEndpoints,
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

/// Write outer headers for L3 GRE encapsulation.
///
/// This function assumes the buffer has reserved space for the inner packet
/// (IP + UDP + payload) and for the outer Ethernet + IP + GRE headers. It does
/// not move or preserve any existing data in `packet`.
fn write_gre_outer_headers(
    packet: &mut [u8],
    gre_src_mac: &MacAddress,
    gre_dst_mac: &MacAddress,
    inner_packet_len: usize,
    gre_header: &GreHeader,
    info: &GreTunnelInfo,
) -> Result<(), PacketError> {
    write_eth_header(packet, &gre_src_mac.0, &gre_dst_mac.0);

    let dont_fragment = info.pmtudisc != 0;

    // Write outer IP header (protocol = GRE = 47)
    let gre_payload_len = GRE_HEADER_BASE_SIZE + inner_packet_len;
    let outer_ttl = (info.ttl != 0).then_some(info.ttl);
    let (IpAddr::V4(outer_local), IpAddr::V4(outer_remote)) = (info.local, info.remote) else {
        return Err(PacketError::InvalidTunnelEndpoints);
    };
    write_ip_header(
        &mut packet[ETH_HEADER_SIZE..],
        &outer_local,
        &outer_remote,
        gre_payload_len as u16,
        IPPROTO_GRE as u8,
        dont_fragment,
        outer_ttl,
        Some(info.tos),
    );

    // Write GRE header
    gre_header.write_to_packet(&mut packet[ETH_HEADER_SIZE + IP_HEADER_SIZE..]);
    Ok(())
}

/// Construct an L3 GRE packet from a UDP payload
///
/// This function takes a UDP payload and constructs a complete L3 GRE packet
/// with the structure: [Ethernet] [Outer IP] [GRE] [Inner IP] [UDP] [Payload]
///
/// On success, writes the constructed GRE packet into `packet` and returns `Ok(())`;
/// returns an error if the buffer is too small or the tunnel info is invalid.
#[allow(clippy::too_many_arguments)]
pub fn construct_gre_packet(
    packet: &mut [u8],
    src_mac: &MacAddress,
    dst_mac: &MacAddress,
    src_ip: &Ipv4Addr,
    dst_ip: &Ipv4Addr,
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
    info: &GreTunnelInfo,
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

    write_gre_outer_headers(
        packet,
        src_mac,
        dst_mac,
        inner_packet_len,
        &gre_header,
        info,
    )?;

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
    Ok(())
}
