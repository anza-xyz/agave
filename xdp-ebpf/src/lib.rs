#![cfg(feature = "agave-unstable-api")]
#![no_std]

#[repr(C, align(8))]
pub struct Aligned<Bytes: ?Sized>(Bytes);

impl<Bytes: ?Sized> core::ops::Deref for Aligned<Bytes> {
    type Target = Bytes;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(all(target_os = "linux", not(target_arch = "bpf")))]
#[unsafe(no_mangle)]
pub static AGAVE_XDP_EBPF_PROGRAM: &Aligned<[u8]> = &Aligned(*include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/agave-xdp-prog"
)));

/// Number of entries in the `FIREWALL_RULES` map, one per UDP port.
pub const FIREWALL_RULES_LEN: u32 = 1 << u16::BITS;

/// Global firewall settings, stored in entry 0 of the `FIREWALL_CONFIG` map.
///
/// The firewall stays disabled while `local_ip` is all zeros, which is the state of the map
/// until user space writes the config.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct FirewallConfig {
    /// IPv4 address (network byte order) that ingress packets must be destined to.
    pub local_ip: [u8; 4],
    /// Strip GRE encapsulation before applying the rules (DoubleZero compatibility).
    pub strip_gre: bool,
    /// Drop packets that violate the rules. When unset, violations are only reported.
    pub enforce: bool,
}

#[cfg(all(target_os = "linux", not(target_arch = "bpf")))]
unsafe impl aya::Pod for FirewallConfig {}

/// Per-port firewall rule, stored in the `FIREWALL_RULES` map indexed by UDP destination port.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum FirewallRule {
    /// Port not managed by the firewall, all packets pass. Zero-initialized map entries hold
    /// this rule, so ports user space did not configure are left alone.
    #[default]
    Unmanaged = 0,
    /// Port is only used for egress, all incoming packets are dropped.
    DenyIngress,
    /// Allows any packet. For ingress ports whose protocol is not validated.
    Allow,
    /// Allows only QUIC packets.
    Quic,
    /// Allows only repair requests.
    Repair,
    /// Allows only shred-sized packets.
    Turbine,
    /// Allows only gossip packets.
    Gossip,
    /// Allows only UDP vote transactions.
    Vote,
}

#[cfg(all(target_os = "linux", not(target_arch = "bpf")))]
unsafe impl aya::Pod for FirewallRule {}

/// Reason a packet was rejected by the firewall.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum RejectReason {
    /// Destination is not the local IP.
    WrongDestinationIp,
    /// Source port is privileged.
    ReservedSourcePort,
    /// Destination port is egress only.
    EgressOnlyPort,
    /// QUIC fixed bit is not set.
    NotQuic,
    /// Payload shorter than a shred, detail is the payload length.
    TurbineTooShort,
    /// Payload shorter than a repair request, detail is the payload length.
    RepairTooShort,
    /// Unexpected `RepairProtocol` discriminant, detail is the first payload byte.
    RepairFingerprint,
    /// Payload shorter than a gossip message, detail is the payload length.
    GossipTooShort,
    /// Unexpected gossip `Protocol` discriminant, detail is the first payload byte.
    GossipFingerprint,
    /// Payload shorter than a vote transaction, detail is the payload length.
    VoteTooShort,
    /// Unexpected vote signature count, detail is the signature count.
    VoteSignatureCount,
}

/// Event reported over the `FIREWALL_EVENTS` ring buffer for every rejected packet.
///
/// Every byte is an initialized field: the verifier rejects copying uninitialized padding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct RejectEvent {
    pub dst_port: u16,
    pub reason: RejectReason,
    pub reserved: u8,
    /// Reason specific value, see [`RejectReason`].
    pub detail: u16,
}

pub const REJECT_EVENT_SIZE: usize = core::mem::size_of::<RejectEvent>();
