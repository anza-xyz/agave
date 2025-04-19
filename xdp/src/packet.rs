#![allow(clippy::arithmetic_side_effects)]

use {libc::ETH_P_IP, std::net::Ipv4Addr};

pub const ETH_HEADER_SIZE: usize = 14;
pub const IP_HEADER_SIZE: usize = 20;
pub const UDP_HEADER_SIZE: usize = 8;

pub fn write_eth_header(packet: &mut [u8], src_mac: &[u8; 6], dst_mac: &[u8; 6]) {
    packet[0..6].copy_from_slice(dst_mac);
    packet[6..12].copy_from_slice(src_mac);
    packet[12..14].copy_from_slice(&(ETH_P_IP as u16).to_be_bytes());
}

pub fn write_ip_header(packet: &mut [u8], src_ip: &Ipv4Addr, dst_ip: &Ipv4Addr, udp_len: u16) {
    let total_len = IP_HEADER_SIZE + udp_len as usize;

    // version (4) and IHL (5)
    packet[0] = 0x45;
    // tos
    packet[1] = 0;
    packet[2..4].copy_from_slice(&(total_len as u16).to_be_bytes());
    // identification
    packet[4..6].copy_from_slice(&0u16.to_be_bytes());
    // flags & frag offset
    packet[6..8].copy_from_slice(&0u16.to_be_bytes());
    // TTL
    packet[8] = 64;
    // protocol (UDP = 17)
    packet[9] = 17;
    // checksum
    packet[10..12].copy_from_slice(&0u16.to_be_bytes());
    packet[12..16].copy_from_slice(&src_ip.octets());
    packet[16..20].copy_from_slice(&dst_ip.octets());

    let checksum = calculate_ip_checksum(&packet[..IP_HEADER_SIZE]);
    packet[10..12].copy_from_slice(&checksum.to_be_bytes());
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
