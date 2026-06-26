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
    std::{collections::HashMap, sync::Arc, time::Duration},
    tokio::sync::watch,
};

/// Snapshot of admitted peers for inbound admission. The value is the peer's
/// epoch stake, which we keep since in most cases the original data structure
/// comes with it already.
pub type AllowlistSnapshot = Arc<HashMap<Pubkey, u64>>;

pub type AllowlistSender = watch::Sender<AllowlistSnapshot>;

pub type AllowlistReceiver = watch::Receiver<AllowlistSnapshot>;

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

/// Capacity of each task -> control-loop connection-event channel.
/// Picked to absorb several slots worth of work.
pub(crate) const CONN_EVENT_CHANNEL_CAP: usize = MAX_ALPENGLOW_VOTE_ACCOUNTS;

/// Per-peer receive side burst limit.
pub const PEER_RATE_LIMIT_BURST: u64 = 100;

/// Per-peer receive side DOS protection limit.
/// If peer exhausts this burst size they are considered to be attacking and their
/// connection is closed.
pub const PEER_RATE_LIMIT_BURST_DOS: u64 = 1000;

/// Sustained rate at which we start inbound TLS handshakes across all peers
/// (handshakes/second). Feeds a token bucket consulted before we call
/// `Incoming::accept()`, so it bounds the rate at which we begin handshake
/// crypto at all, not how many run at once.
pub const HANDSHAKE_GLOBAL_RATE: f64 = MAX_ALPENGLOW_VOTE_ACCOUNTS as f64;

/// Burst of inbound handshakes tolerated above [`HANDSHAKE_GLOBAL_RATE`] before
/// new attempts are shed. Sized to ensure load spikes are smoothed.
pub(crate) const HANDSHAKE_BURST: u64 = 500;

/// Maximum inbound TLS handshakes allowed in flight at once. Once this many are
/// pending we stop pulling new attempts off the endpoint until one finishes.
/// This is the limiter on handshake memory use; the rate at which we *start*
/// handshakes is smoothed separately by [`HANDSHAKE_GLOBAL_RATE`].
pub const MAX_INFLIGHT_HANDSHAKES: usize = MAX_ALPENGLOW_VOTE_ACCOUNTS;

/// Hard timeout for inbound handshake. During handshakes connections
/// may be kept alive, this explicit cap (mirroring the streamer transport)
/// reclaims the handshake slots regardless of what the peer sends.
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);

/// How often expired banlist entries are pruned.
pub(crate) const BANLIST_PRUNE_INTERVAL: Duration = Duration::from_hours(1);

/// How often endpoint metrics are reported.
pub(crate) const METRICS_INTERVAL: Duration = Duration::from_secs(1);

/// ALPN protocol identifier for the Alpenglow votor datagram transport.
pub const ALPENGLOW_ALPN: &[u8] = b"alpenglow-v1";
