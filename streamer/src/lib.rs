#![cfg(feature = "agave-unstable-api")]
#![allow(clippy::arithmetic_side_effects)]
pub mod nonblocking;
pub mod quic;
pub mod quic_socket;

pub use agave_cluster_transport::{evicting_sender, packet, recvmmsg, sendmmsg};

pub mod streamer {
    pub use agave_cluster_transport::{
        packet::filter_packets_by_socket_addr_space, receiver::*, staked_nodes::StakedNodes,
    };
}

#[macro_use]
extern crate log;

#[macro_use]
extern crate solana_metrics;
