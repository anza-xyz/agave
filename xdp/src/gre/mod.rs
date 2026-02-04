//! L3 GRE tunnel support for XDP egress
//!
//! This module provides L3 GRE encapsulation, which wraps IPv4 packets only (not Ethernet frames).
//! Only a single GRE interface is supported; routing and caches assume one GRE tunnel.
//!
//! The GRE tunnel structure: [Ethernet] [Outer IP] [GRE] [Inner IP] [UDP] [Payload]
//!
//! Note: This module currently only supports the basic GRE header (version 0),
//! without optional checksum, key, or sequence number fields (C, K, S flags).
//! The flag bits are preserved in the representation for future extensibility.
//!
//! References:
//! - RFC 2784: Generic Routing Encapsulation (GRE)
//! - RFC 2890: Key and Sequence Number Extensions to GRE
pub mod packet;

pub use packet::{construct_gre_packet, gre_packet_size, GreHeader, PacketError};
