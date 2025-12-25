//! L3 GRE (Generic Routing Encapsulation) tunnel support for XDP egress
//!
//! This module provides L3 GRE encapsulation, which wraps IP packets (not Ethernet frames).
//! L3 GRE is distinct from L2 GRE (GREtap) which encapsulates Ethernet frames.
//!
//! The GRE tunnel structure: [Ethernet] [Outer IP] [GRE] [Inner IP] [UDP] [Payload]

pub mod encapsulator;
pub mod packet;

pub use {
    encapsulator::{EncapsulationError, GreEncapsulator},
    packet::{construct_gre_packet, GreHeader, PacketError},
};
