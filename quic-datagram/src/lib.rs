#![doc = include_str!("../README.md")]
#![cfg(feature = "agave-unstable-api")]

pub mod allowlist;
pub mod error;

#[cfg(any(test, feature = "dev-context-only-utils"))]
pub mod testutils;

pub(crate) mod client;
pub(crate) use error::close_codes;
pub mod endpoint;
pub(crate) mod server;
pub(crate) mod stats;
pub(crate) mod transport;

use std::time::Duration;

/// Maximum number of unique peer pubkeys we expect in steady state.
/// Used to size buffers and channels, actual peer count is controlled
/// by admission policy.
///
/// The authoritative version of this lives in
/// `solana_runtime::bank::MAX_ALPENGLOW_VOTE_ACCOUNTS`; this is a deliberate
/// copy to avoid a `solana-runtime` dependency from this transport crate.
pub const MAX_ALPENGLOW_VOTE_ACCOUNTS: usize = 2000;

/// Maximum simultaneous inbound (we-accepted, receive-only) connections we keep
/// from a single peer pubkey. Two is needed to let a hot-spare of the same
/// identity to connect while the previous instance's connection is still alive.
pub const MAX_INBOUND_CONNECTIONS_PER_PEER: usize = 2;

/// Capacity of the egress channel held by [`QuicDatagramEndpoint`].
///
/// Sized to absorb a full slot's burst with headroom -
/// `MAX_ALPENGLOW_VOTE_ACCOUNTS` peers × ~4 messages/slot = ~8 K items.
pub const EGRESS_CHANNEL_CAP: usize = 4 * MAX_ALPENGLOW_VOTE_ACCOUNTS;

/// Per-peer receive-side rate limit.
pub const MAX_DATAGRAMS_PER_SECOND_PER_PEER: f64 = 30.0;

/// Per-peer receive side burst limit.
pub const PEER_RATE_LIMIT_BURST: u64 = 100;

/// Per-peer receive side DOS protection limit.
/// If peer exhausts this burst size they are considered to be attacking and banned.
pub const PEER_RATE_LIMIT_BURST_DOS: u64 = 100000;

/// Maximum simultaneous TLS handshakes in flight across all peers.
/// Acts as the burst ceiling for [`HANDSHAKE_GLOBAL_RATE`].
pub const HANDSHAKE_GLOBAL_BURST: u64 = 200;

/// Sustained inbound TLS handshake rate across all peers (handshakes/second).
pub const HANDSHAKE_GLOBAL_RATE: f64 = 800.0;

/// How often each connection's read loop re-checks whether the peer is still
/// in the allowlist and closes any remaining connections.
pub const ALLOWLIST_CHECK_INTERVAL: Duration = Duration::from_secs(10);

/// How often expired banlist entries are pruned.
pub(crate) const BANLIST_PRUNE_INTERVAL: Duration = Duration::from_hours(1);

/// How often endpoint metrics are reported.
pub(crate) const METRICS_INTERVAL: Duration = Duration::from_secs(1);

/// ALPN protocol identifier for the Alpenglow votor datagram transport.
pub const ALPENGLOW_ALPN: &[u8] = b"alpenglow-v1";
