#![allow(clippy::arithmetic_side_effects)]

#[cfg(feature = "agave-unstable-api")]
mod addr_cache;

#[cfg(feature = "agave-unstable-api")]
pub mod broadcast_stage;

#[cfg(feature = "agave-unstable-api")]
pub mod cluster_nodes;

#[cfg(feature = "agave-unstable-api")]
pub mod quic_endpoint;

#[cfg(feature = "agave-unstable-api")]
pub mod retransmit_stage;

#[cfg(feature = "agave-unstable-api")]
pub mod sigverify_shreds;

#[cfg(feature = "agave-unstable-api")]
pub mod xdp;

#[cfg(feature = "agave-unstable-api")]
#[macro_use]
extern crate log;

#[cfg(feature = "agave-unstable-api")]
#[macro_use]
extern crate solana_metrics;

#[cfg(test)]
#[macro_use]
extern crate assert_matches;
