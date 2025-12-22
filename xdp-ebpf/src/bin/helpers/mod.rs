use {
    agave_xdp_ebpf::RejectReason,
    aya_ebpf::programs::XdpContext,
    core::mem,
    network_types::{
        eth::{EthHdr, EtherType},
        ip::{IpProto, Ipv4Hdr},
        udp::UdpHdr,
    },
};

pub mod vote;

/// Basic GRE header without checksum, key or sequence number.
const GRE_HDR_LEN: usize = 4;
/// GRE protocol type of an encapsulated IPv4 packet.
const GRE_PROTO_IPV4: [u8; 2] = 0x0800u16.to_be_bytes();
/// IPv4 header length in bytes when there are no IP options.
const IPV4_HDR_LEN_NO_OPTIONS: u8 = 20;

const OUTER_IP_OFFSET: usize = EthHdr::LEN;
const GRE_OFFSET: usize = OUTER_IP_OFFSET + Ipv4Hdr::LEN;
const INNER_IP_OFFSET: usize = GRE_OFFSET + GRE_HDR_LEN;
const OUTER_UDP_OFFSET: usize = OUTER_IP_OFFSET + Ipv4Hdr::LEN;
const INNER_UDP_OFFSET: usize = INNER_IP_OFFSET + Ipv4Hdr::LEN;

/// Smallest shred carried by turbine.
const MIN_SHRED_LEN: usize = 1200;
/// `REPAIR_REQUEST_MIN_BYTES` in serve_repair.
const MIN_REPAIR_REQUEST_LEN: usize = 128;
/// `RepairProtocol` variants handled by serve repair, `Pong..=WindowIndexForBlockId`.
const REPAIR_PROTOCOL_DISCRIMINANTS: core::ops::RangeInclusive<u8> = 7..=14;
/// Smallest gossip message, a ping/pong.
const MIN_GOSSIP_LEN: usize = 132;
/// Largest gossip `Protocol` discriminant.
const MAX_GOSSIP_DISCRIMINANT: u8 = 5;

/// Outcome of a firewall check, `Ok` lets the packet through.
pub type Verdict = Result<(), Reject>;

#[derive(Clone, Copy)]
pub struct Reject {
    pub reason: RejectReason,
    /// Reason specific value, see [`RejectReason`].
    pub detail: u16,
}

impl Reject {
    #[inline(always)]
    pub fn new(reason: RejectReason, detail: u16) -> Self {
        Self { reason, detail }
    }
}

/// Why a packet's headers could not be extracted.
pub enum ExtractError {
    /// Not an IPv4 packet (ARP, IPv6, ...).
    NotIpv4,
    /// IPv4 packet without a UDP header we can locate: other protocols, IP options, non-first
    /// fragments or GRE we do not strip.
    NotUdp,
    /// Packet is shorter than its headers.
    Truncated,
}

pub struct ExtractedHeader {
    /// Destination IPv4 address in network byte order.
    pub dst_ip: [u8; 4],
    pub src_port: u16,
    pub dst_port: u16,
    pub payload_offset: usize,
    pub payload_len: usize,
}

impl ExtractedHeader {
    #[inline(always)]
    pub fn from_context(ctx: &XdpContext, strip_gre: bool) -> Result<Self, ExtractError> {
        let ethhdr: *const EthHdr = ptr_at(ctx, 0)?;
        // SAFETY: ptr_at checked that the header is within the packet
        if unsafe { (*ethhdr).ether_type } != u16::from(EtherType::Ipv4) {
            return Err(ExtractError::NotIpv4);
        }
        let mut ipv4hdr: *const Ipv4Hdr = ptr_at(ctx, OUTER_IP_OFFSET)?;
        // SAFETY: ptr_at checked that the header is within the packet
        let mut ip_proto = unsafe { checked_ip_proto(&*ipv4hdr) }?;
        let mut udp_offset = OUTER_UDP_OFFSET;
        if strip_gre && ip_proto == u8::from(IpProto::Gre) {
            let grehdr: *const [u8; GRE_HDR_LEN] = ptr_at(ctx, GRE_OFFSET)?;
            // SAFETY: ptr_at checked that the header is within the packet
            let [flags0, flags1, proto0, proto1] = unsafe { *grehdr };
            if flags0 != 0 || flags1 != 0 || [proto0, proto1] != GRE_PROTO_IPV4 {
                return Err(ExtractError::NotUdp);
            }
            ipv4hdr = ptr_at(ctx, INNER_IP_OFFSET)?;
            // SAFETY: ptr_at checked that the header is within the packet
            ip_proto = unsafe { checked_ip_proto(&*ipv4hdr) }?;
            udp_offset = INNER_UDP_OFFSET;
        }
        if ip_proto != u8::from(IpProto::Udp) {
            return Err(ExtractError::NotUdp);
        }
        let udphdr: *const UdpHdr = ptr_at(ctx, udp_offset)?;
        // SAFETY: ptr_at checked that both headers are within the packet
        let (dst_ip, src_port, dst_port, udp_len) = unsafe {
            (
                (*ipv4hdr).dst_addr,
                (*udphdr).src_port(),
                (*udphdr).dst_port(),
                (*udphdr).len(),
            )
        };
        Ok(Self {
            dst_ip,
            src_port,
            dst_port,
            payload_offset: udp_offset.saturating_add(UdpHdr::LEN),
            payload_len: usize::from(udp_len).saturating_sub(UdpHdr::LEN),
        })
    }
}

/// Returns the protocol of an IPv4 packet whose payload starts right after the fixed header.
#[inline(always)]
fn checked_ip_proto(ipv4hdr: &Ipv4Hdr) -> Result<u8, ExtractError> {
    // the payload of non-first fragments has no transport header
    if ipv4hdr.ihl() != IPV4_HDR_LEN_NO_OPTIONS || ipv4hdr.frag_offset() != 0 {
        return Err(ExtractError::NotUdp);
    }
    Ok(ipv4hdr.proto)
}

/// Returns a pointer to a `T` at `offset` into the packet, if the packet is long enough.
#[inline(always)]
// packet addresses and offsets are far from overflowing
#[allow(clippy::arithmetic_side_effects)]
pub fn ptr_at<T>(ctx: &XdpContext, offset: usize) -> Result<*const T, ExtractError> {
    let (start, end) = (ctx.data(), ctx.data_end());
    if start + offset + mem::size_of::<T>() > end {
        return Err(ExtractError::Truncated);
    }
    Ok((start + offset) as *const T)
}

/// Reads a `T` at `offset` into the packet, if the packet is long enough.
#[inline(always)]
pub fn read_at<T: Copy>(ctx: &XdpContext, offset: usize) -> Option<T> {
    let ptr = ptr_at::<T>(ctx, offset).ok()?;
    // SAFETY: ptr_at checked that the value is within the packet
    Some(unsafe { ptr.read_unaligned() })
}

#[inline(always)]
pub fn check_quic_fingerprint(first_byte: Option<u8>) -> Verdict {
    // the fixed bit is set in both long and short QUIC v1 headers
    match first_byte {
        Some(first_byte) if first_byte & 0x40 != 0 => Ok(()),
        _ => Err(Reject::new(RejectReason::NotQuic, 0)),
    }
}

#[inline(always)]
pub fn check_turbine_size(header: &ExtractedHeader) -> Verdict {
    if header.payload_len < MIN_SHRED_LEN {
        return Err(Reject::new(
            RejectReason::TurbineTooShort,
            header.payload_len as u16,
        ));
    }
    Ok(())
}

#[inline(always)]
pub fn check_repair_fingerprint(header: &ExtractedHeader, first_byte: Option<u8>) -> Verdict {
    if header.payload_len < MIN_REPAIR_REQUEST_LEN {
        return Err(Reject::new(
            RejectReason::RepairTooShort,
            header.payload_len as u16,
        ));
    }
    match first_byte {
        Some(first_byte) if REPAIR_PROTOCOL_DISCRIMINANTS.contains(&first_byte) => Ok(()),
        first_byte => Err(Reject::new(
            RejectReason::RepairFingerprint,
            first_byte.unwrap_or_default().into(),
        )),
    }
}

#[inline(always)]
pub fn check_gossip_fingerprint(header: &ExtractedHeader, first_byte: Option<u8>) -> Verdict {
    if header.payload_len < MIN_GOSSIP_LEN {
        return Err(Reject::new(
            RejectReason::GossipTooShort,
            header.payload_len as u16,
        ));
    }
    match first_byte {
        Some(first_byte) if first_byte <= MAX_GOSSIP_DISCRIMINANT => Ok(()),
        first_byte => Err(Reject::new(
            RejectReason::GossipFingerprint,
            first_byte.unwrap_or_default().into(),
        )),
    }
}
