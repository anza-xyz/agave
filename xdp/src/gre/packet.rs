#![allow(clippy::arithmetic_side_effects)]

use {
    crate::packet::{
        write_eth_header, write_ip_header, write_ip_header_for_udp, write_udp_header,
        ETH_HEADER_SIZE, GRE_HEADER_SIZE, IP_HEADER_SIZE, UDP_HEADER_SIZE,
    },
    libc::{ETH_P_IP, IPPROTO_GRE},
    std::net::Ipv4Addr,
    thiserror::Error,
};

pub const INNER_PACKET_HEADER_SIZE: usize = IP_HEADER_SIZE + UDP_HEADER_SIZE;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PacketError {
    #[error("packet buffer too small: need {needed} bytes, have {have} bytes")]
    BufferTooSmall { needed: usize, have: usize },
}

/// GRE header structure
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct GreHeader {
    /// Flags (4 bits) + Version (3 bits) + Reserved (9 bits)
    pub flags_version: u16,
    /// Protocol
    pub protocol: u16,
}

impl GreHeader {
    /// Create a new GRE header with default values
    pub fn new(protocol_type: u16) -> Self {
        Self {
            flags_version: 0, // Flags: 0, Version: 0, Reserved: 0
            protocol: protocol_type,
        }
    }

    /// Write the GRE header to a packet buffer
    pub fn write_to_packet(&self, packet: &mut [u8]) {
        packet[0..2].copy_from_slice(&self.flags_version.to_be_bytes());
        packet[2..4].copy_from_slice(&self.protocol.to_be_bytes());
    }
}

/// Wrap a packet with L3 GRE encapsulation
///
/// Takes the original packet (IP + UDP + payload) and wraps it
/// with Ethernet + IP + GRE headers, creating:
/// [Ethernet] [Outer IP] [GRE] [Inner IP] [UDP] [Payload]
///
/// This is L3 GRE (encapsulates IP packets), not L2 GRE/GREtap (which encapsulates Ethernet frames).
fn wrap_packet_with_gre(
    packet: &mut [u8],
    gre_src_ip: Ipv4Addr,
    gre_dst_ip: Ipv4Addr,
    gre_src_mac: &[u8; 6],
    gre_dst_mac: &[u8; 6],
    inner_packet_len: usize, // Length of IP + UDP + payload (no Ethernet)
) -> usize {
    // Calculate new packet size
    let gre_packet_size = ETH_HEADER_SIZE + IP_HEADER_SIZE + GRE_HEADER_SIZE + inner_packet_len;

    // Write Ethernet header
    write_eth_header(packet, gre_src_mac, gre_dst_mac);

    // Write outer IP header (protocol = GRE = 47)
    let gre_payload_len = GRE_HEADER_SIZE + inner_packet_len;
    write_ip_header(
        &mut packet[ETH_HEADER_SIZE..],
        &gre_src_ip,
        &gre_dst_ip,
        gre_payload_len as u16,
        IPPROTO_GRE as u8,
        true,
    );

    // Write GRE header
    let gre_header = GreHeader::new(ETH_P_IP as u16);
    gre_header.write_to_packet(&mut packet[ETH_HEADER_SIZE + IP_HEADER_SIZE..]);

    gre_packet_size
}

/// Construct an L3 GRE packet from a UDP payload
///
/// This function takes a UDP payload and constructs a complete L3 GRE packet
/// with the structure: [Ethernet] [Outer IP] [GRE] [Inner IP] [UDP] [Payload]
///
/// This is L3 GRE (encapsulates IP packets), not L2 GRE/GREtap (which encapsulates Ethernet frames).
///
/// # Arguments
/// * `packet` - Buffer to write the GRE packet into
/// * `src_ip` - Source IP for the inner packet
/// * `dst_ip` - Destination IP for the inner packet
/// * `src_port` - Source port for the inner UDP packet
/// * `dst_port` - Destination port for the inner UDP packet
/// * `payload` - The UDP payload data
/// * `gre_src_ip` - GRE tunnel source IP
/// * `gre_dst_ip` - GRE tunnel destination IP
/// * `src_mac` - Source MAC address
/// * `dst_mac` - Destination MAC address
///
/// # Returns
/// The total length of the constructed GRE packet, or an error if the buffer is too small
#[allow(clippy::too_many_arguments)]
pub fn construct_gre_packet(
    packet: &mut [u8],
    src_ip: &Ipv4Addr,
    dst_ip: &Ipv4Addr,
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
    gre_src_ip: Ipv4Addr,
    gre_dst_ip: Ipv4Addr,
    src_mac: &[u8; 6],
    dst_mac: &[u8; 6],
) -> Result<usize, PacketError> {
    let payload_len = payload.len();

    // Calculate sizes
    let inner_packet_len = INNER_PACKET_HEADER_SIZE + payload_len;
    let gre_packet_size = ETH_HEADER_SIZE + IP_HEADER_SIZE + GRE_HEADER_SIZE + inner_packet_len;

    // Ensure packet buffer is large enough
    if packet.len() < gre_packet_size {
        return Err(PacketError::BufferTooSmall {
            needed: gre_packet_size,
            have: packet.len(),
        });
    }

    // Write the inner packet (IP + UDP + payload) at the GRE payload position
    let inner_start = ETH_HEADER_SIZE + IP_HEADER_SIZE + GRE_HEADER_SIZE;

    // Write inner IP header
    write_ip_header_for_udp(
        &mut packet[inner_start..inner_start + IP_HEADER_SIZE],
        src_ip,
        dst_ip,
        (UDP_HEADER_SIZE + payload_len) as u16,
    );

    // Write UDP header
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

    // Wrap with GRE headers
    Ok(wrap_packet_with_gre(
        packet,
        gre_src_ip,
        gre_dst_ip,
        src_mac,
        dst_mac,
        inner_packet_len,
    ))
}
