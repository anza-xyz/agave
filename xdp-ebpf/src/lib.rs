#![cfg_attr(
    not(feature = "agave-unstable-api"),
    deprecated(
        since = "3.1.0",
        note = "This crate has been marked for formal inclusion in the Agave Unstable API. From \
                v4.0.0 onward, the `agave-unstable-api` crate feature must be specified to \
                acknowledge use of an interface that may break without warning."
    )
)]
// Activate some of the Rust 2024 lints to make the future migration easier.
#![warn(if_let_rescope)]
#![warn(keyword_idents_2024)]
#![warn(rust_2024_incompatible_pat)]
#![warn(tail_expr_drop_order)]
#![warn(unsafe_attr_outside_unsafe)]
#![warn(unsafe_op_in_unsafe_fn)]
#![no_std]

use core::net::Ipv4Addr;

#[cfg(all(target_os = "linux", not(target_arch = "bpf")))]
#[unsafe(no_mangle)]
pub static AGAVE_XDP_EBPF_PROGRAM: &[u8] =
    aya::include_bytes_aligned!(concat!(env!("CARGO_MANIFEST_DIR"), "/agave-xdp-prog"));

#[cfg_attr(
    all(target_os = "linux", not(target_arch = "bpf")),
    derive(serde::Deserialize)
)]
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct FirewallConfig {
    /// Base port for all Solana services
    pub solana_min_port: u16,
    /// Bind IP of the validator
    pub my_ip: Ipv4Addr,
    ///Strips GRE headers for DZ compatibility
    pub strip_gre: bool,
    /// Drop on fragmented contexts
    pub drop_frags: bool,
    /// Whether to enforce firewall rules
    /// Logic interpreting drop_frags is always active
    pub enforce: bool,
}

pub const MAX_FIREWALL_RULES: u16 = 64;

#[cfg_attr(
    all(target_os = "linux", not(target_arch = "bpf")),
    derive(serde::Deserialize)
)]
#[derive(Default, Clone, Copy, Debug)]
#[repr(u8)]
pub enum FirewallRule {
    /// Drops all incoming packets
    #[default]
    DenyIngress,
    /// Allows only QUIC packets
    Quic,
    /// Allows packets of Repair protocol
    Repair,
    /// Allows only UDP shreds
    Turbine,
    /// Allows only gossip
    Gossip,
    /// Allows only UDP vote packets
    Vote,
}

#[cfg(all(target_os = "linux", not(target_arch = "bpf")))]
unsafe impl aya::Pod for FirewallRule {}

impl Default for FirewallConfig {
    fn default() -> Self {
        Self {
            solana_min_port: 0,
            my_ip: Ipv4Addr::UNSPECIFIED,
            strip_gre: true,
            drop_frags: false,
            enforce: false,
        }
    }
}

#[cfg(all(target_os = "linux", not(target_arch = "bpf")))]
unsafe impl aya::Pod for FirewallConfig {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirewallDecision {
    Pass,

    RepairTooShort(u16),
    AncestorRepairTooShort(u16),
    RepairFingerprint(u8),
    ServeRepairTooShort(u16),

    GossipFingerprint,
    GossipTooShort,

    IpWrongDestination,
    ReservedPort,
    TxOnlyPort(u16),

    NotQuicPacket,
    TpuQuicTooShort,
    TpuVoteQuicTooShort,

    TurbineTooShort(u16),

    VoteTooShort,
    VoteInvalidSigCount(u8),
}

#[cfg(all(target_os = "linux", not(target_arch = "bpf")))]
unsafe impl aya::Pod for FirewallDecision {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DecisionEvent {
    pub dst_port: u16,
    pub decision: FirewallDecision,
}

pub const DECISION_EVENT_SIZE: usize = core::mem::size_of::<DecisionEvent>();
