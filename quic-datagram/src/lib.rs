#![doc = include_str!("../README.md")]
#![cfg(feature = "agave-unstable-api")]

pub mod allowlist;
pub mod error;

#[cfg(any(test, feature = "dev-context-only-utils"))]
pub mod testutils;

pub(crate) mod client;
pub(crate) mod close_codes;
pub(crate) mod connection_table;
pub mod endpoint;
pub mod key_updater;
pub(crate) mod read_loop;
pub(crate) mod server;
pub(crate) mod stats;
pub(crate) mod subnet_rate_limit;
pub(crate) mod transport;

use std::time::Duration;
pub use {
    allowlist::{Allowlist, StakedNodesAllowlist},
    endpoint::QuicDatagramEndpoint,
    error::Error,
    key_updater::KeyUpdater,
    solana_net_utils::banlist::Banlist,
};

/// Ban duration applied by the crate's only internal trigger: HANDOVER
/// reception in the per-connection read loop. Short window because the
/// peer may legitimately be retrying a hot-spare promotion.
pub const BAN_DURATION_SHORT: Duration = Duration::from_secs(5);

/// Maximum number of unique peer pubkeys held in the connection table.
/// Sized for the maximum expected alpenglow staked-node count.
pub const MAX_PEERS: usize = 2000;

/// Capacity of the egress channel held by [`QuicDatagramEndpoint`]. Senders
/// must `try_send` and accept drop-on-full.
///
/// Sized to absorb a full slot's burst with headroom - `MAX_PEERS` peers
/// × ~4 messages/slot = ~8 K items, doubled for safety against bursty
/// drainers. Production steady-state pressure is far lower (per-validator
/// vote rate is ~1 message/slot).
pub const EGRESS_CHANNEL_CAP: usize = 16384;

/// Per-peer receive-side rate limit. Each connection read loop
/// enforces RX rate via a token bucket; bucket starts full at
/// connection open and refills continously. Any datagram arriving
/// when the bucket has no tokens left is dropped.
pub const MAX_DATAGRAMS_PER_SECOND_PER_PEER: f64 = 30.0;

/// Per-peer receive side burst limit. Complimentary to
/// [`MAX_DATAGRAMS_PER_SECOND_PER_PEER`], allows for bursty arrivals
pub const BURST_DATAGRAMS_PER_SECOND_PER_PEER: u64 = 100;
