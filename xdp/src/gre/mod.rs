//! L3 GRE tunnel support for XDP egress
//!
//! This module provides L3 GRE encapsulation, which wraps IPv4 packets only (not Ethernet frames).
//!
//! The GRE tunnel structure: [Ethernet] [Outer IP] [GRE] [Inner IP] [UDP] [Payload]
//!
pub mod encapsulator;
pub mod packet;

pub use {
    encapsulator::{EncapsulationError, GreEncapsulator},
    packet::{construct_gre_packet, GreConfig, GreHeader, PacketError},
};
