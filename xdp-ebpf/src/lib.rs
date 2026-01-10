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
    /// Ports on which only egress is allowed
    pub deny_ingress_ports: [u16; 8],
    pub tpu_vote: u16,
    pub tpu_quic: u16,
    pub tpu_forwards_quic: u16,
    pub tpu_vote_quic: u16,
    pub turbine: u16,
    pub repair: u16,
    pub serve_repair: u16,
    pub ancestor_repair: u16,
    pub gossip: u16,
    pub solana_min_port: u16,
    pub solana_max_port: u16,
    pub my_ip: Ipv4Addr,
    ///Strips GRE headers for DZ compatibility
    pub strip_gre: bool,
    pub drop_frags: bool,
    /// Whether to enforce firewall rules
    pub enforce: bool,
}

impl Default for FirewallConfig {
    fn default() -> Self {
        Self {
            deny_ingress_ports: [0; 8],
            tpu_vote: 0,
            tpu_quic: 0,
            tpu_forwards_quic: 0,
            tpu_vote_quic: 0,
            turbine: 0,
            repair: 0,
            serve_repair: 0,
            ancestor_repair: 0,
            gossip: 0,
            solana_min_port: 0,
            solana_max_port: 0,
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
