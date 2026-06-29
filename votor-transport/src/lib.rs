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
pub type PeerListSnapshot = Arc<HashMap<Pubkey, SocketAddr>>;

pub type PeerListSender = watch::Sender<PeerListSnapshot>;

pub type PeerListReceiver = watch::Receiver<PeerListSnapshot>;

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

/// Votor should pace broadcasts to each peer. This window is the
/// instantaneous burst headroom (`window × nominal rate`) we allow to
/// absorb votor's transient bursts before we start dropping at ingress.
/// 1 second is enough to absorb plausible network jitter and implementation
/// differences.
pub const PEER_RATE_LIMIT_BURST_WINDOW: Duration = Duration::from_secs(1);
/// A peer that sustains above-nominal rate long enough to drain this much
/// budget is obviously not following votor's self-pacing, so we close the
/// connection rather than just shaping. Chosen as 10x of base burst window
/// as a balance between probability of dropping legit peer and allowing broken
/// peers to keep sending.
pub const PEER_RATE_LIMIT_DOS_WINDOW: Duration = Duration::from_secs(10);

/// Sustained rate at which we start inbound TLS handshakes (handshakes/second),
/// consulted before `Endpoint::accept()` so it bounds when we begin handshake
/// crypto at all. Sized to the validator set: the whole set may (re)connect at
/// once after a coordinated restart and should be admitted within ~1 second.
pub const HANDSHAKE_GLOBAL_RATE: f64 = MAX_ALPENGLOW_VOTE_ACCOUNTS as f64;

/// Burst of inbound handshakes tolerated above [`HANDSHAKE_GLOBAL_RATE`] before
/// new attempts are shed: 200ms of the sustained rate. Kept small to cap the
/// synchronous handshake-crypto spike on the accept loop.
pub(crate) const HANDSHAKE_BURST: u64 = 400;

/// How many instances of AcceptLoop to spawn per server endpoint. This controls
/// the max number of cores we dedicate to processing of the TLS handshakes, per endpoint.
pub(crate) const HANDSHAKE_WORKERS_PER_ENDPOINT: usize = 1;

/// Maximum inbound handshakes allowed in flight; once reached we stop pulling
/// new ones off the endpoint. Bounds handshake memory use. Sized to the
/// validator set, which may be handshaking all at once after a restart.
pub const MAX_INFLIGHT_HANDSHAKES: usize = MAX_ALPENGLOW_VOTE_ACCOUNTS;

/// Hard timeout for an inbound handshake, operates regardless of what
/// the peer sends. ~1s suffices for a 300ms-RTT handshake with no packet
/// loss -> 2s to have margin for retransmits.
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);

/// How often expired banlist entries are pruned. Bans last for hours,
/// and their duration is chosen by BLS sigverify, this prune only
/// reclaims hashmap slots for pubkeys which are no longer banned, i.e.
/// it never affects whether a ban is active or not.
pub(crate) const BANLIST_PRUNE_INTERVAL: Duration = Duration::from_mins(1);

/// How often endpoint metrics are reported.
pub(crate) const METRICS_INTERVAL: Duration = Duration::from_secs(1);

/// ALPN protocol identifier for the Alpenglow votor datagram transport.
pub const ALPENGLOW_ALPN: &[u8] = b"alpenglow-v1";
