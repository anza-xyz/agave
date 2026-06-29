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
    std::{
        collections::HashMap,
        net::{IpAddr, Ipv4Addr, SocketAddr},
        sync::Arc,
        time::Duration,
    },
    tokio::sync::watch,
};

/// Sentinel address for a peer we want to admit but do not yet know the address of.
pub const ADDRESS_UNKNOWN: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);

/// Snapshot of desired peers list. Peers we cannot yet resolve via gossip
/// carry the [`ADDRESS_UNKNOWN`] sentinel.
///
/// Inbound admission uses membership only (the address is ignored).
type PeerListSnapshot = Arc<HashMap<Pubkey, SocketAddr>>;

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

/// Allows one backup instance to connect while connection from the primary instance
/// has not yet timed out (this is normal during validator updates).
/// More than two is erroneous and implies multiple backup nodes assumed the same
/// identity, we ignore extra connections until existing ones time out.
pub const MAX_INBOUND_CONNECTIONS_PER_PEER: usize = 2;

/// Capacity of the channel used by the accept and read tasks to report
/// connection lifecycle events. Events are per-connection, so sized
/// to absorb a whole cluster disconnecting at once.
pub(crate) const CONN_EVENT_CHANNEL_CAP: usize =
    MAX_ALPENGLOW_VOTE_ACCOUNTS * MAX_INBOUND_CONNECTIONS_PER_PEER;

/// Votor should pace broadcasts to each peer. We must drop packets that
/// break the limits set by BLS sigverifier. However, network jitter may
/// add some bursts in the delivery, so this window is the maximum amount
/// of time we allow the packets to be piling up at max rate before we start
/// dropping at ingress. 1 second is enough to absorb likely network
/// jitter and should be safe with current BLS sigverifier implementation.
pub const PEER_RATE_LIMIT_BURST_WINDOW: Duration = Duration::from_secs(1);
/// A peer that sustains above-nominal rate long enough to drain this much
/// budget is obviously not following votor's self-pacing, so we close the
/// connection, since datagrams have no flow control.
/// Chosen as 10 * [`PEER_RATE_LIMIT_BURST_WINDOW`] as a balance between
/// probability of dropping legit peer and allowing broken peers to keep
/// sending unbounded amount of traffic.
pub const PEER_RATE_LIMIT_DOS_WINDOW: Duration = Duration::from_secs(10);

/// Sustained rate at which we start inbound TLS handshakes (handshakes/second),
/// consulted before `Endpoint::accept()` so it bounds when we begin handshake
/// crypto at all. Sized to the validator set: the whole set may (re)connect at
/// once after a coordinated restart and should be admitted within ~1 second.
pub const HANDSHAKE_GLOBAL_RATE: f64 = MAX_ALPENGLOW_VOTE_ACCOUNTS as f64;

/// Burst of inbound handshakes tolerated above allowed rate before
/// new attempts are shed. Chosen to align with 200ms at [`HANDSHAKE_GLOBAL_RATE`].
/// Kept small to cap the handshake-crypto load spikes on the accept loop.
pub(crate) const HANDSHAKE_BURST: u64 = 400;

/// How many instances of AcceptLoop to spawn per server endpoint. This controls
/// the max number of cores we dedicate to processing of the TLS handshakes, per endpoint.
pub(crate) const HANDSHAKE_WORKERS_PER_ENDPOINT: usize = 1;

/// Maximum inbound handshakes allowed in flight; once reached we stop pulling
/// new ones off the endpoint. Bounds handshake memory use. Sized to the
/// validator set, which may be handshaking all at once after a restart.
pub const MAX_INFLIGHT_HANDSHAKES: usize = MAX_ALPENGLOW_VOTE_ACCOUNTS;

/// Hard timeout for an inbound handshake, enforced regardless of what
/// the peer sends. ~1s suffices for a 300ms-RTT handshake with no packet
/// loss, so we use 2s to have margin for losses & retransmits.
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);

/// How often endpoint metrics are reported.
pub(crate) const METRICS_INTERVAL: Duration = Duration::from_secs(1);

/// ALPN protocol identifier for the Alpenglow votor datagram transport.
pub const ALPENGLOW_ALPN: &[u8] = b"alpenglow-v1";
