//! L3 GRE (Generic Routing Encapsulation) tunnel support for XDP egress
//!
//! This module provides L3 GRE encapsulation, which wraps IP packets (not Ethernet frames).
//! L3 GRE is distinct from L2 GRE (GREtap) which encapsulates Ethernet frames.
//!
//! The GRE tunnel structure: [Ethernet] [Outer IP] [GRE] [Inner IP] [UDP] [Payload]
//!
//! # Assumptions
//!
//! - **IPv4 only**: Currently supports IPv4 for both inner and outer IP headers
//! - **L3 GRE only**: Encapsulates IP packets, not Ethernet frames (GREtap)
//! - **Protocol type**: Assumes GRE protocol type is IPv4 (ETH_P_IP)

pub mod encapsulator;
pub mod packet;

pub use {
    encapsulator::{EncapsulationError, GreEncapsulator},
    packet::{construct_gre_packet, GreConfig, GreHeader, PacketError},
};
