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
pub const MAX_PEERS: u64 = 2000;

/// Maximum simultaneous inbound (we-accepted, receive-only) connections we keep
/// from a single peer pubkey. Two is needed to let a hot-spare of the same
/// identity to connect while the previous instance's connection is still alive.
pub const MAX_INBOUND_CONNECTIONS_PER_PEER: usize = 2;

/// Capacity of the egress channel held by [`QuicDatagramEndpoint`].
///
/// Sized to absorb a full slot's burst with headroom - `MAX_PEERS` peers
/// × ~4 messages/slot = ~8 K items.
pub const EGRESS_CHANNEL_CAP: usize = 4 * MAX_PEERS as usize;

/// Per-peer receive-side rate limit.
pub const MAX_DATAGRAMS_PER_SECOND_PER_PEER: f64 = 30.0;

/// Per-peer receive side burst limit.
pub const PEER_RATE_LIMIT_BURST: u64 = 100;

/// Per-peer receive side DOS protection limit.
/// If peer exhausts this burst size they are considered to be attacking and banned.
pub const PEER_RATE_LIMIT_BURST_DOS: u64 = 100000;

/// If peer exceeds DOS budget they get banned.
pub const BAN_DURATION_DOS: Duration = Duration::from_hours(48);

/// Maximum simultaneous TLS handshakes in flight across all peers.
/// Acts as the burst ceiling for [`HANDSHAKE_GLOBAL_RATE`].
pub const HANDSHAKE_GLOBAL_BURST: u64 = 200;

/// Sustained inbound TLS handshake rate across all peers (handshakes/second).
pub const HANDSHAKE_GLOBAL_RATE: f64 = 400.0;

/// How often each connection's read loop re-checks whether the peer is still
/// in the allowlist and closes any remaining connections.
pub const ALLOWLIST_CHECK_INTERVAL: Duration = Duration::from_secs(10);

/// An outbound connection untouched by upstream for this long is closed.
pub const EGRESS_IDLE_REAP_THRESHOLD: Duration = Duration::from_mins(10);

/// ALPN protocol identifier for the Alpenglow votor datagram transport.
pub const ALPENGLOW_ALPN: &[u8] = b"alpenglow-v1";
