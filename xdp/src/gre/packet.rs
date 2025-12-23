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
// GRE header size without optional fields
pub const GRE_HEADER_BASE_SIZE: usize = 4;
// GRE key field size (optional, present when K flag is set)
pub const GRE_KEY_SIZE: usize = 4;

/// GRE tunnel configuration for packet construction
///
/// This struct contains all the parameters needed to construct a GRE-encapsulated packet.
/// It's independent of any specific tunnel discovery mechanism (e.g., netlink).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GreConfig {
    /// Local (source) IP address for the GRE tunnel outer header
    pub local: Ipv4Addr,
    /// Remote (destination) IP address for the GRE tunnel outer header
    pub remote: Ipv4Addr,
    /// GRE output flags (IFLA_GRE_OFLAGS) - flags to set in GRE header
    pub oflags: Option<u16>,
    /// GRE output key (IFLA_GRE_OKEY) - optional key for GRE header
    pub okey: Option<u32>,
    /// TTL for outer IP header (IFLA_GRE_TTL)
    pub ttl: Option<u8>,
    /// TOS for outer IP header (IFLA_GRE_TOS)
    pub tos: Option<u8>,
    /// PMTU discovery setting (IFLA_GRE_PMTUDISC)
    /// - Some(0) = PMTUD enabled (DF bit set)
    /// - Some(1) = PMTUD disabled (DF bit cleared)
    /// - None = default (DF bit set)
    pub pmtudisc: Option<u8>,
}

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
    /// Common flags: C (0x8000), R (0x4000), K (0x2000), S (0x1000)
    pub flags_version: u16,
    /// Protocol
    pub protocol: u16,
    /// Optional key (present when K flag is set in flags_version)
    pub key: Option<u32>,
}

impl GreHeader {
    /// Create a new GRE header with specified flags
    /// `oflags` should contain the GRE flags (e.g., TUNNEL_KEY, TUNNEL_CSUM, etc.)
    /// `okey` is the optional key value (if present, K flag will be set automatically)
    pub fn new(protocol_type: u16, oflags: Option<u16>, okey: Option<u32>) -> Self {
        let mut flags_version = oflags.unwrap_or(0);

        // If key is present, ensure K flag (0x2000) is set
        if okey.is_some() {
            flags_version |= 0x2000; // GRE_KEY flag
        }

        Self {
            flags_version,
            protocol: protocol_type,
            key: okey,
        }
    }

    /// Get the size of this GRE header in bytes
    pub fn size(&self) -> usize {
        GRE_HEADER_BASE_SIZE + if self.key.is_some() { GRE_KEY_SIZE } else { 0 }
    }

    /// Write the GRE header to a packet buffer
    /// Returns the number of bytes written
    pub fn write_to_packet(&self, packet: &mut [u8]) -> usize {
        packet[0..2].copy_from_slice(&self.flags_version.to_be_bytes());
        packet[2..4].copy_from_slice(&self.protocol.to_be_bytes());

        let mut offset = GRE_HEADER_BASE_SIZE;
        if let Some(key) = self.key {
            packet[offset..offset + GRE_KEY_SIZE].copy_from_slice(&key.to_be_bytes());
            offset += GRE_KEY_SIZE;
        }

        offset
    }
}

impl GreConfig {
    /// Create a new GRE configuration with required endpoints
    pub fn new(local: Ipv4Addr, remote: Ipv4Addr) -> Self {
        Self {
            local,
            remote,
            oflags: None,
            okey: None,
            ttl: None,
            tos: None,
            pmtudisc: None,
        }
    }
}

/// Conversion from netlink's GreTunnelInfo to GreConfig
///
/// This allows the GRE packet construction to work with tunnel info discovered via netlink,
/// while keeping the core GRE module independent of netlink.
impl TryFrom<&crate::netlink::GreTunnelInfo> for GreConfig {
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
            oflags: info.oflags,
            okey: info.okey,
            ttl: info.ttl,
            tos: info.tos,
            pmtudisc: info.pmtudisc,
        })
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
    gre_src_mac: &[u8; 6],
    gre_dst_mac: &[u8; 6],
    inner_packet_len: usize, // Length of IP + UDP + payload (no Ethernet)
    config: &GreConfig,
) -> usize {
    // Create GRE header to determine its size
    let gre_header = GreHeader::new(ETH_P_IP as u16, config.oflags, config.okey);
    let gre_header_size = gre_header.size();

    // Calculate new packet size
    let gre_packet_size = ETH_HEADER_SIZE + IP_HEADER_SIZE + gre_header_size + inner_packet_len;

    // Write Ethernet header
    write_eth_header(packet, gre_src_mac, gre_dst_mac);

    // Determine DF bit: if PMTUDISC is disabled (1), clear DF bit
    let dont_fragment = config.pmtudisc.map(|v| v == 0).unwrap_or(true);

    // Write outer IP header (protocol = GRE = 47)
    let gre_payload_len = gre_header_size + inner_packet_len;
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
/// * `src_mac` - Source MAC address
/// * `dst_mac` - Destination MAC address
/// * `config` - GRE tunnel configuration (local/remote IPs and tunnel parameters)
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
    src_mac: &[u8; 6],
    dst_mac: &[u8; 6],
    config: &GreConfig,
) -> Result<usize, PacketError> {
    let payload_len = payload.len();

    // Determine GRE header size (may include optional key)
    let gre_header = GreHeader::new(ETH_P_IP as u16, config.oflags, config.okey);
    let gre_header_size = gre_header.size();

    // Calculate sizes
    let inner_packet_len = INNER_PACKET_HEADER_SIZE + payload_len;
    let gre_packet_size = ETH_HEADER_SIZE + IP_HEADER_SIZE + gre_header_size + inner_packet_len;

    // Ensure packet buffer is large enough
    if packet.len() < gre_packet_size {
        return Err(PacketError::BufferTooSmall {
            needed: gre_packet_size,
            have: packet.len(),
        });
    }

    // Write the inner packet (IP + UDP + payload) at the GRE payload position
    let inner_start = ETH_HEADER_SIZE + IP_HEADER_SIZE + gre_header_size;

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
        src_mac,
        dst_mac,
        inner_packet_len,
        config,
    ))
}
