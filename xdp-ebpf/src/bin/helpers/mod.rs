use {
    agave_xdp_ebpf::FirewallDecision,
    aya_ebpf::programs::XdpContext,
    core::{mem, net::Ipv4Addr},
    network_types::{
        eth::{EthHdr, EtherType},
        ip::{IpProto, Ipv4Hdr},
        udp::UdpHdr,
    },
};

pub mod vote;

const GRE_HDR_LEN: usize = 4;

#[inline(always)]
pub fn parse_ip_header(ipv4hdr: *const Ipv4Hdr) -> (Ipv4Addr, Ipv4Addr, IpProto) {
    let src_ip = Ipv4Addr::from_bits(unsafe { u32::from_be_bytes((*ipv4hdr).src_addr) });
    let dst_ip = Ipv4Addr::from_bits(unsafe { u32::from_be_bytes((*ipv4hdr).dst_addr) });
    let ip_proto = unsafe { (*ipv4hdr).proto };
    (src_ip, dst_ip, ip_proto)
}

pub enum ExtractError {
    /// Special marker for packets we do not support (so we do not know if we should drop them)
    NotSupported,
    /// Some valid IP packet but not UDP, we should let it pass
    NotUdp,
    /// Malformed Ethernet frame we can not parse
    MalformedEth,
    /// Malformed packets we can not parse
    InvalidOffset,
}

pub struct ExtractedHeader {
    pub payload_offset: usize,
    pub _src_ip: Ipv4Addr,
    pub dst_ip: Ipv4Addr,
    pub src_port: u16,
    pub dst_port: u16,
    pub payload_len: usize,
}
impl ExtractedHeader {
    pub fn from_context(ctx: &XdpContext, strip_gre: bool) -> Result<Self, ExtractError> {
        let ethhdr: *const EthHdr = ptr_at(&ctx, 0)?;
        match unsafe {
            (*ethhdr)
                .ether_type()
                .map_err(|_| ExtractError::MalformedEth)?
        } {
            EtherType::Ipv4 => {}
            _ => return Err(ExtractError::NotSupported),
        }
        let mut ip_header_offset = EthHdr::LEN;
        let mut ipv4hdr: *const Ipv4Hdr = ptr_at(&ctx, ip_header_offset)?;
        let (src_ip, dst_ip, ip_proto) = parse_ip_header(ipv4hdr);
        let (src_ip, dst_ip) = match (ip_proto, strip_gre) {
            (IpProto::Udp, _) => (src_ip, dst_ip),
            (IpProto::Gre, true) => {
                ip_header_offset = EthHdr::LEN + Ipv4Hdr::LEN + GRE_HDR_LEN;

                ipv4hdr = ptr_at(&ctx, ip_header_offset)?;
                let (src_ip, dst_ip, ip_proto) = parse_ip_header(ipv4hdr);
                if !matches!(ip_proto, IpProto::Udp) {
                    return Err(ExtractError::NotUdp);
                }
                (src_ip, dst_ip)
            }
            _ => {
                return Err(ExtractError::NotUdp);
            }
        };
        let udphdr: *const UdpHdr = ptr_at(&ctx, ip_header_offset + Ipv4Hdr::LEN)?;
        let src_port = unsafe { u16::from_be_bytes((*udphdr).src) };
        let dst_port = unsafe { u16::from_be_bytes((*udphdr).dst) };
        let payload_len =
            unsafe { (u16::from_be_bytes((*udphdr).len) as usize).saturating_sub(UdpHdr::LEN) };
        let payload_offset = ip_header_offset + Ipv4Hdr::LEN + UdpHdr::LEN;
        Ok(Self {
            payload_offset,
            _src_ip: src_ip,
            dst_ip,
            src_port,
            dst_port,
            payload_len,
        })
    }
}

#[inline(always)]
pub fn ptr_at<T>(ctx: &XdpContext, offset: usize) -> Result<*const T, ExtractError> {
    let (start, end) = (ctx.data(), ctx.data_end());
    let len = mem::size_of::<T>();

    if start + offset + len > end {
        return Err(ExtractError::InvalidOffset);
    }

    let ptr = (start + offset) as *const T;
    Ok(ptr)
}

#[inline(always)]
pub fn get_or_default<T: Default + Copy>(ctx: &XdpContext, offset: usize) -> T {
    match ptr_at(&ctx, offset) {
        Ok(ptr) => unsafe { *ptr },
        Err(_) => return Default::default(),
    }
}

#[inline(always)]
pub fn check_quic_fingerprint(first_byte: u8) -> FirewallDecision {
    if (first_byte & 0x40) == 0 {
        FirewallDecision::NotQuicPacket
    } else {
        FirewallDecision::Pass
    }
}

pub fn check_repair_fingerprint(header: &ExtractedHeader, first_byte: u8) -> FirewallDecision {
    //defined by REPAIR_REQUEST_MIN_BYTES
    if header.payload_len < 128 {
        return FirewallDecision::RepairTooShort(header.payload_len as u16);
    }
    // based on RepairProtocol enum
    if !(6..=11).contains(&first_byte) {
        return FirewallDecision::RepairFingerprint(first_byte);
    }
    FirewallDecision::Pass
}

pub fn check_gossip_fingerprint(header: &ExtractedHeader, first_byte: u8) -> FirewallDecision {
    if header.payload_len < 132 {
        return FirewallDecision::GossipTooShort;
    }
    if first_byte > 5 {
        return FirewallDecision::GossipFingerprint;
    }

    FirewallDecision::Pass
}
