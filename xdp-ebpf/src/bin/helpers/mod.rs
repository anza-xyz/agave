use {
    agave_xdp_ebpf::Flags,
    aya_ebpf::{helpers::gen::bpf_xdp_get_buff_len, maps::Array, programs::XdpContext},
    core::{mem, net::Ipv4Addr},
    network_types::{
        eth::{EthHdr, EtherType},
        ip::{IpProto, Ipv4Hdr},
        udp::UdpHdr,
    },
};

const GRE_HDR_LEN: usize = 4;

pub fn parse_ip_header(ipv4hdr: *const Ipv4Hdr) -> (Ipv4Addr, Ipv4Addr, IpProto) {
    let src_ip = Ipv4Addr::from_bits(unsafe { u32::from_be_bytes((*ipv4hdr).src_addr) });
    let dst_ip = Ipv4Addr::from_bits(unsafe { u32::from_be_bytes((*ipv4hdr).dst_addr) });
    let ip_proto = unsafe { (*ipv4hdr).proto };
    (src_ip, dst_ip, ip_proto)
}

pub enum ExtractError {
    Ipv6,
    NotUdp,
    Drop,
}

pub struct ExtractedHeader {
    pub ip_header_offset: usize,
    pub src_ip: Ipv4Addr,
    pub dst_ip: Ipv4Addr,
    pub src_port: u16,
    pub dst_port: u16,
    pub len: usize,
}
impl ExtractedHeader {
    pub fn from_context(ctx: &XdpContext, flags: Flags) -> Result<Self, ExtractError> {
        let ethhdr: *const EthHdr = unsafe { ptr_at(&ctx, 0)? };
        match unsafe { (*ethhdr).ether_type().map_err(|_| ExtractError::Drop)? } {
            EtherType::Ipv4 => {}
            EtherType::Ipv6 => return Err(ExtractError::Ipv6),
            _ => return Err(ExtractError::Drop),
        }
        let mut ip_header_offset = EthHdr::LEN;
        let mut ipv4hdr: *const Ipv4Hdr = unsafe { ptr_at(&ctx, ip_header_offset)? };
        let (src_ip, dst_ip, ip_proto) = parse_ip_header(ipv4hdr);
        let (src_ip, dst_ip) = match (ip_proto, flags) {
            (IpProto::Udp, Flags::Default) | (IpProto::Udp, Flags::StripGre) => (src_ip, dst_ip),
            (IpProto::Gre, Flags::OnlyGre) => {
                ip_header_offset = EthHdr::LEN + Ipv4Hdr::LEN + GRE_HDR_LEN;

                ipv4hdr = unsafe { ptr_at(&ctx, ip_header_offset)? };
                let (src_ip, dst_ip, ip_proto) = parse_ip_header(ipv4hdr);
                if !matches!(ip_proto, IpProto::Udp) {
                    return Err(ExtractError::NotUdp);
                }
                (src_ip, dst_ip)
            }
            (IpProto::Gre, Flags::StripGre) => {
                ip_header_offset = EthHdr::LEN + Ipv4Hdr::LEN + GRE_HDR_LEN;

                ipv4hdr = unsafe { ptr_at(&ctx, ip_header_offset)? };
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
        let udphdr: *const UdpHdr = unsafe { ptr_at(&ctx, ip_header_offset + Ipv4Hdr::LEN)? };
        let src_port = unsafe { u16::from_be_bytes((*udphdr).src) };
        let dst_port = unsafe { u16::from_be_bytes((*udphdr).dst) };
        let len = unsafe { (u16::from_be_bytes((*udphdr).len) as usize) + 20 };

        Ok(Self {
            ip_header_offset,
            src_ip,
            dst_ip,
            src_port,
            dst_port,
            len,
        })
    }
}

pub fn array_has_entry<T: Eq>(haystack: &Array<T>, length: u32, needle: T) -> bool {
    for idx in 0..length {
        match haystack.get(idx) {
            Some(v) => {
                if *v == needle {
                    return true;
                }
            }
            None => return false,
        }
    }
    false
}

#[inline(always)]
pub unsafe fn ptr_at<T>(ctx: &XdpContext, offset: usize) -> Result<*const T, ExtractError> {
    let (start, end) = (ctx.data(), ctx.data_end());
    let len = mem::size_of::<T>();

    if start + offset + len > end {
        return Err(ExtractError::Drop);
    }

    let ptr = (start + offset) as *const T;
    Ok(unsafe { &*ptr })
}

#[inline]
pub fn has_frags(ctx: &XdpContext) -> bool {
    #[allow(clippy::arithmetic_side_effects)]
    let linear_len = ctx.data_end() - ctx.data();
    // Safety: generated binding is unsafe, but static verifier guarantees ctx.ctx is valid.
    let buf_len = unsafe { bpf_xdp_get_buff_len(ctx.ctx) as usize };
    linear_len < buf_len
}
