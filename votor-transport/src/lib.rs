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

pub struct PeerList {
    /// Peers with whom we are interested in communicating. Inbound admission
    /// uses membership only, the address is only needed to push.
    pub peers: HashMap<Pubkey, Option<SocketAddr>>,
    /// Whether we open outbound connections.
    pub push_enabled: bool,
}

/// Readers can clone the Arc and release the watch lock immediately.
type PeerListSnapshot = Arc<PeerList>;

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

/// Upper bound on unique peers admitted into the peer_list at once.
///
/// `PeerListUpdater::refresh_peer_list` can union the root epoch's staked-node
/// set with one adjacent epoch's: the trailing epoch near the start of a new
/// epoch, or the next epoch near the end of the current one. Those two
/// windows don't overlap, so at most two epochs' sets (never three) are ever
/// unioned outside of pathological startup conditions that don't occur on
/// mainnet. With realistic validator-set churn between those two epochs the
/// union can somewhat exceed a single epoch's cap, but nowhere near double
/// it, since most validators remain staked across the boundary. 1.5x budgets
/// for that expected churn without grossly over-provisioning.
pub const MAX_PEER_LIST_SIZE: usize = MAX_ALPENGLOW_VOTE_ACCOUNTS * 3 / 2;

/// Allows one backup instance to connect while connection from the primary instance
/// has not yet timed out (this is normal during validator updates).
/// More than two is erroneous and implies multiple backup nodes assumed the same
/// identity, we ignore extra connections until existing ones time out.
pub const MAX_INBOUND_CONNECTIONS_PER_PEER: usize = 2;

/// Capacity of the channel used by the accept and read tasks to report
/// connection lifecycle events. Events are per-connection, so sized
/// to absorb a whole cluster disconnecting at once, including the epoch-churn
/// overlap captured by [`MAX_PEER_LIST_SIZE`].
pub(crate) const CONN_EVENT_CHANNEL_CAP: usize =
    MAX_PEER_LIST_SIZE * MAX_INBOUND_CONNECTIONS_PER_PEER;

/// Votor should pace broadcasts to each peer. We must drop packets that
/// break the limits set by BLS sigverifier. However, network jitter may
/// add some bursts in the delivery, so this window is the maximum amount
/// of time we allow the packets to be piling up at max rate before we start
/// dropping at ingress. 1 second is enough to absorb likely network
/// jitter and should be safe with current BLS sigverifier implementation.
pub const PEER_RATE_LIMIT_BURST_WINDOW: Duration = Duration::from_secs(1);
/// A peer that sustains above max allowed rate long enough to drain this much
/// budget is obviously not following votor's self-pacing, so we close the
/// connection, since datagrams have no flow control.
/// Chosen as 10 * [`PEER_RATE_LIMIT_BURST_WINDOW`] as a balance between
/// probability of dropping legit peer and allowing broken peers to keep
/// sending unbounded amount of traffic.
pub const PEER_RATE_LIMIT_DOS_WINDOW: Duration = Duration::from_secs(10);

/// Sustained rate at which we start inbound TLS handshakes (handshakes/second),
/// consulted before `Endpoint::accept()` so it bounds when we begin handshake
/// crypto at all. Sized to the validator set, which may (re)connect all at once
/// after a coordinated restart.
///
/// This is a ceiling, not a rate we sustain. Whenever attempts stop completing
/// promptly [`MAX_INFLIGHT_HANDSHAKES`] may get saturated first, so the achievable
/// rate is [`HANDSHAKE_DRAIN_RATE`]. Size queues from that constant, not this one.
pub const HANDSHAKE_GLOBAL_RATE: usize = MAX_ALPENGLOW_VOTE_ACCOUNTS;

/// Burst of inbound handshakes tolerated above allowed rate before
/// new attempts are shed. Chosen to align with 200ms at [`HANDSHAKE_GLOBAL_RATE`].
/// Kept small to cap the handshake-crypto load spikes on the accept loop.
pub(crate) const HANDSHAKE_BURST: u64 = 400;

/// How many instances of AcceptLoop to spawn per server endpoint. This controls
/// the max number of cores we dedicate to processing of the TLS handshakes, per endpoint.
pub(crate) const HANDSHAKE_WORKERS_PER_ENDPOINT: usize = 1;

/// Hard timeout for an inbound handshake, enforced regardless of what
/// the peer sends. ~1s suffices for a 300ms-RTT handshake with no packet
/// loss, so we use 2s to have margin for losses & retransmits.
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);

/// Maximum inbound handshakes allowed in flight; once reached we stop pulling
/// new ones off the endpoint. Bounds handshake memory use. Sized to the most
/// handshakes the rate limiter can admit within one [`HANDSHAKE_TIMEOUT`]
/// under sustained [`HANDSHAKE_GLOBAL_RATE`] load, so admission is governed by
/// the rate limiter rather than by this cap.
pub const MAX_INFLIGHT_HANDSHAKES: usize =
    HANDSHAKE_GLOBAL_RATE * HANDSHAKE_TIMEOUT.as_millis() as usize / 1000;

/// Rate at which the accept loops actually take connection attempts off the
/// endpoints under sustained load, aggregated over all endpoints
/// (handshakes/second).
pub(crate) const HANDSHAKE_DRAIN_RATE: usize = {
    let inflight_bound = MAX_INFLIGHT_HANDSHAKES * 1000 / (HANDSHAKE_TIMEOUT.as_millis() as usize);
    if inflight_bound < HANDSHAKE_GLOBAL_RATE {
        inflight_bound
    } else {
        HANDSHAKE_GLOBAL_RATE
    }
};

/// Longest an inbound attempt that we are going to serve may sit queued inside
/// quinn before an accept loop reaches it. This is what sizes quinn's `max_incoming`:
/// a deeper queue does not admit more handshakes (the drain rate is fixed), it only
/// adds latency to the attempts we do serve, so we keep it short and let the excess
/// be shed by quinn.
pub(crate) const MAX_INCOMING_DELAY: Duration =
    Duration::from_millis(HANDSHAKE_BURST * 1000 / HANDSHAKE_GLOBAL_RATE as u64);

/// How often endpoint metrics are reported.
pub(crate) const METRICS_INTERVAL: Duration = Duration::from_secs(1);

/// ALPN protocol identifier for the Alpenglow votor datagram transport.
pub(crate) const ALPENGLOW_ALPN: &[u8] = b"alpenglow-v1";

/// Maximum reasonable number of QUIC endpoints to allow.
pub const MAX_ENDPOINTS: usize = 8;
