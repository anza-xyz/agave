#![cfg(feature = "agave-unstable-api")]

pub mod error;

pub(crate) mod client;
pub(crate) use error::close_codes;
pub mod endpoint;
pub(crate) mod server;
pub(crate) mod stats;
pub(crate) mod transport;

use {
    solana_pubkey::Pubkey,
    std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration},
    tokio::sync::watch,
};

/// Snapshot of desired peers. Peers we cannot yet resolve via gossip
/// carry the sentinel IP set to `0.0.0.0:0`.
///
/// Inbound admission uses membership only (the address is ignored).
pub type PeerlistSnapshot = Arc<HashMap<Pubkey, SocketAddr>>;

pub type PeerlistSender = watch::Sender<PeerlistSnapshot>;

pub type PeerlistReceiver = watch::Receiver<PeerlistSnapshot>;

/// Maximum number of unique peer pubkeys we expect in steady state.
/// Used to size buffers and channels, actual peer count is controlled
/// by the peer list.
///
/// The authoritative version of this lives in
/// `solana_runtime::bank::MAX_ALPENGLOW_VOTE_ACCOUNTS`; this is a deliberate
/// copy to avoid a `solana-runtime` dependency from this transport crate.
pub const MAX_ALPENGLOW_VOTE_ACCOUNTS: usize = 2000;

/// Maximum simultaneous inbound connections we keep from a single peer pubkey.
/// Two are needed to let a hot-spare of the same identity connect while the
/// previous instance's connection is still alive.
pub const MAX_INBOUND_CONNECTIONS_PER_PEER: usize = 2;

/// Capacity of each task -> control-loop connection-event channel.
/// Picked to absorb several slots worth of work.
pub(crate) const CONN_EVENT_CHANNEL_CAP: usize = MAX_ALPENGLOW_VOTE_ACCOUNTS;

/// Tolerance to burst as window over the nominal per-peer send rate.
pub const PEER_RATE_LIMIT_BURST_WINDOW: Duration = Duration::from_secs(2);
/// Used to compute number of packets to allow at over-limit rate before dropping
/// the connection.
pub const PEER_RATE_LIMIT_DOS_WINDOW: Duration = Duration::from_secs(20);

/// Sustained rate at which we start inbound TLS handshakes in (handshakes/second).
/// This is consulted before we call `Endpoint::accept()`, so it bounds the rate
/// at which we begin any kind of handshake crypto at all.
pub const HANDSHAKE_GLOBAL_RATE: f64 = MAX_ALPENGLOW_VOTE_ACCOUNTS as f64;

/// Burst of inbound handshakes tolerated above [`HANDSHAKE_GLOBAL_RATE`] before
/// new attempts are shed. Sized to ensure that reasonable load spikes are tolerated.
pub(crate) const HANDSHAKE_BURST: u64 = 500;

/// Maximum inbound handshakes allowed in flight. Once this many are
/// pending we stop pulling new attempts off the endpoint.
/// This is the limiter on handshake memory use.
pub const MAX_INFLIGHT_HANDSHAKES: usize = MAX_ALPENGLOW_VOTE_ACCOUNTS;

/// Hard timeout for inbound handshake. During handshakes connections
/// may be kept alive, this explicit cap reclaims the resources
/// regardless of what the peer sends.
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);

/// How often expired banlist entries are pruned.
pub(crate) const BANLIST_PRUNE_INTERVAL: Duration = Duration::from_hours(1);

/// How often endpoint metrics are reported.
pub(crate) const METRICS_INTERVAL: Duration = Duration::from_secs(1);

/// ALPN protocol identifier for the Alpenglow votor datagram transport.
pub const ALPENGLOW_ALPN: &[u8] = b"alpenglow-v1";
