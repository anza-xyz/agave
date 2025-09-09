#![allow(clippy::arithmetic_side_effects)]

use {
    libc::{ETH_P_IP, IPPROTO_GRE, IPPROTO_UDP},
    std::net::Ipv4Addr,
};

pub const ETH_HEADER_SIZE: usize = 14;
pub const IP_HEADER_SIZE: usize = 20;
pub const UDP_HEADER_SIZE: usize = 8;
pub const GRE_HEADER_SIZE: usize = 4;
const IP_DONT_FRAGMENT: u16 = 0x4000;

pub fn write_eth_header(packet: &mut [u8], src_mac: &[u8; 6], dst_mac: &[u8; 6]) {
    packet[0..6].copy_from_slice(dst_mac);
    packet[6..12].copy_from_slice(src_mac);
    packet[12..14].copy_from_slice(&(ETH_P_IP as u16).to_be_bytes());
}

fn write_ip_header(
    packet: &mut [u8],
    src_ip: &Ipv4Addr,
    dst_ip: &Ipv4Addr,
    payload_len: u16,
    protocol: u8,
    dont_fragment: bool,
) {
    let total_len = IP_HEADER_SIZE + payload_len as usize;

    // version (4) and IHL (5)
    packet[0] = 0x45;
    // tos
    packet[1] = 0;
    packet[2..4].copy_from_slice(&(total_len as u16).to_be_bytes());
    // identification
    packet[4..6].copy_from_slice(&0u16.to_be_bytes());
    // flags & frag offset
    let frag_flags = if dont_fragment { IP_DONT_FRAGMENT } else { 0 };
    packet[6..8].copy_from_slice(&frag_flags.to_be_bytes()); //todo: greg: do we need this for all ip headers? FD has it in theirs
                                                             // TTL
    packet[8] = 64;
    // protocol
    packet[9] = protocol;
    // checksum
    packet[10..12].copy_from_slice(&0u16.to_be_bytes());
    packet[12..16].copy_from_slice(&src_ip.octets());
    packet[16..20].copy_from_slice(&dst_ip.octets());

    let checksum = calculate_ip_checksum(&packet[..IP_HEADER_SIZE]);
    packet[10..12].copy_from_slice(&checksum.to_be_bytes());
}

/// Write IP header configured for UDP protocol
pub fn write_ip_header_for_udp(
    packet: &mut [u8],
    src_ip: &Ipv4Addr,
    dst_ip: &Ipv4Addr,
    payload_len: u16,
) {
    write_ip_header(
        packet,
        src_ip,
        dst_ip,
        payload_len,
        IPPROTO_UDP as u8,
        false,
    );
}

pub fn write_udp_header(
    packet: &mut [u8],
    src_ip: &Ipv4Addr,
    src_port: u16,
    dst_ip: &Ipv4Addr,
    dst_port: u16,
    payload_len: u16,
    csum: bool,
) {
    let udp_len = UDP_HEADER_SIZE + payload_len as usize;

    packet[0..2].copy_from_slice(&src_port.to_be_bytes());
    packet[2..4].copy_from_slice(&dst_port.to_be_bytes());
    packet[4..6].copy_from_slice(&(udp_len as u16).to_be_bytes());
    packet[6..8].copy_from_slice(&0u16.to_be_bytes());

    if csum {
        let checksum = calculate_udp_checksum(&packet[..udp_len], src_ip, dst_ip);
        packet[6..8].copy_from_slice(&checksum.to_be_bytes());
    }
}

fn calculate_udp_checksum(udp_packet: &[u8], src_ip: &Ipv4Addr, dst_ip: &Ipv4Addr) -> u16 {
    let udp_len = udp_packet.len();

    let mut sum: u32 = 0;

    let src_ip = src_ip.octets();
    let dst_ip = dst_ip.octets();

    sum += (u32::from(src_ip[0]) << 8) | u32::from(src_ip[1]);
    sum += (u32::from(src_ip[2]) << 8) | u32::from(src_ip[3]);
    sum += (u32::from(dst_ip[0]) << 8) | u32::from(dst_ip[1]);
    sum += (u32::from(dst_ip[2]) << 8) | u32::from(dst_ip[3]);
    sum += 17; // UDP
    sum += udp_len as u32;

    for i in 0..udp_len / 2 {
        // skip the checksum field
        if i * 2 == 6 {
            continue;
        }
        let word = ((udp_packet[i * 2] as u32) << 8) | (udp_packet[i * 2 + 1] as u32);
        sum += word;
    }

    if udp_len % 2 == 1 {
        sum += (udp_packet[udp_len - 1] as u32) << 8;
    }

    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }

    !(sum as u16)
}

fn calculate_ip_checksum(header: &[u8]) -> u16 {
    let mut sum: u32 = 0;

    for i in 0..header.len() / 2 {
        let word = ((header[i * 2] as u32) << 8) | (header[i * 2 + 1] as u32);
        sum += word;
    }

    if header.len() % 2 == 1 {
        sum += (header[header.len() - 1] as u32) << 8;
    }

    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }

    !(sum as u16)
}

/// GRE header structure
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct GreHeader {
    /// Flags (4 bits) + Version (3 bits) + Reserved (9 bits)
    pub flags_version: u16,
    /// Protocol - what's being encapsulated (e.g., ETH_P_IP)
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

/// Wrap a packet with GRE encapsulation
///
/// Takes the original packet (IP + UDP + payload) and wraps it
/// with Ethernet + IP + GRE headers, creating:
/// [Ethernet] [Outer IP] [GRE] [Inner IP] [UDP] [Payload]
pub fn wrap_packet_with_gre(
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
    let gre_header = GreHeader::new(libc::ETH_P_IP as u16);
    gre_header.write_to_packet(&mut packet[ETH_HEADER_SIZE + IP_HEADER_SIZE..]);

    gre_packet_size
}

// Construct a GRE packet from a UDP payload
///
/// This function takes a UDP payload and constructs a complete GRE packet
/// with the structure: [Ethernet] [Outer IP] [GRE] [Inner IP] [UDP] [Payload]
///
/// # Arguments
/// * `packet` - Buffer to write the GRE packet into
/// * `src_ip` - Source IP for the inner packet -> this is the interfaces IP addres from doublezero0
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
/// The total length of the constructed GRE packet
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
) -> usize {
    let payload_len = payload.len();

    // Calculate sizes
    const INNER_PACKET_HEADER_SIZE: usize = IP_HEADER_SIZE + UDP_HEADER_SIZE;
    let inner_packet_len = INNER_PACKET_HEADER_SIZE + payload_len;
    let gre_packet_size = ETH_HEADER_SIZE + IP_HEADER_SIZE + GRE_HEADER_SIZE + inner_packet_len;

    // Ensure packet buffer is large enough
    if packet.len() < gre_packet_size {
        panic!("Packet buffer too small for GRE packet");
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
    wrap_packet_with_gre(
        packet,
        gre_src_ip,
        gre_dst_ip,
        src_mac,
        dst_mac,
        inner_packet_len,
    )
}
