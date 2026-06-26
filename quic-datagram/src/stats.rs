use {
    crate::error::Error,
    quinn::{ConnectionError, SendDatagramError},
    solana_metrics::datapoint_info,
    std::sync::atomic::{AtomicU64, Ordering},
};

#[derive(Default)]
pub struct QuicDatagramStats {
    // --- Positive path ---
    /// High-water mark of live table entries over the reporting period.
    pub(crate) peak_connections: AtomicU64,
    /// Datagrams successfully delivered to the ingress channel.
    pub(crate) datagrams_received: AtomicU64,
    /// Datagrams successfully handed to quinn for transmission.
    pub(crate) datagrams_sent: AtomicU64,
    /// A connection ended through an expected teardown path.
    pub(crate) connection_lost: AtomicU64,
    /// Egress dropped because the connection for the peer is not ready.
    pub(crate) egress_dropped_connect_in_progress: AtomicU64,

    // --- Offered load (inbound accept / handshake) ---
    /// Inbound handshakes we began TLS work for.
    pub(crate) handshakes_started: AtomicU64,
    /// Inbound TLS handshakes that completed successfully and yielded an
    /// authenticated connection.
    pub(crate) handshakes_completed: AtomicU64,

    // --- Connection lifecycle management ---
    /// Connections closed because the local identity (TLS cert / pubkey) was
    /// rotated. Each rotation evicts every cached connection in one burst.
    pub(crate) connection_evicted_identity_rotated: AtomicU64,
    /// Live inbound connection closed because the periodic allowlist recheck
    /// found the peer no longer admitted (e.g. evicted at an epoch boundary).
    pub(crate) connection_evicted_allowlist: AtomicU64,
    /// The outbound side evicted a cached connection because the caller
    /// supplied a new socket addr for the same pubkey (peer moved).
    pub(crate) connection_evicted_peer_moved: AtomicU64,

    // --- Error buckets ---
    /// A connection failed abnormally: connect setup error, protocol-level
    /// connection fault, or a non-transient `send_datagram` failure. All are
    /// local/config/protocol faults that should not occur in prod for a
    /// well-behaved peer.
    pub(crate) connect_failed: AtomicU64,
    /// Handshake refused because the peer is not permitted: not in the
    /// allowlist, or currently banned.
    pub(crate) handshake_rejected_unauthorized: AtomicU64,
    /// Handshake refused due to a resource limit: connection table full.
    pub(crate) handshake_rejected_overload: AtomicU64,
    /// Number of times the accept gate closed because the global handshake
    /// rate limit was exhausted, pausing how fast we pull new attempts off the
    /// endpoint.
    pub(crate) handshake_rate_limited: AtomicU64,
    /// Inbound handshake torn down because it did not complete within
    /// `HANDSHAKE_TIMEOUT` (a stalled or actively-stalling peer).
    pub(crate) handshake_timed_out: AtomicU64,
    /// Peer's incoming datagram exceeded the per-connection rate.
    pub(crate) datagram_rate_limited: AtomicU64,

    // --- Drops on internal channels (distinct backpressure signals) ---
    /// We have received a datagram but have nowhere to put it
    pub(crate) datagram_ingress_dropped_channel_full: AtomicU64,
}

#[inline]
pub(crate) fn add(metric: &AtomicU64) {
    metric.fetch_add(1, Ordering::Relaxed);
}

impl QuicDatagramStats {
    /// Updates the peak-occupancy after a successful connection install.
    pub(crate) fn record_connection_count(&self, count: u64) {
        self.peak_connections.fetch_max(count, Ordering::Relaxed);
    }
}

pub(crate) fn record_error(err: &Error, stats: &QuicDatagramStats) {
    match err {
        Error::Connection(
            ConnectionError::ApplicationClosed(_)
            | ConnectionError::ConnectionClosed(_)
            | ConnectionError::LocallyClosed
            | ConnectionError::Reset
            | ConnectionError::TimedOut,
        )
        | Error::SendDatagram(SendDatagramError::ConnectionLost(_)) => add(&stats.connection_lost),
        Error::Connect(_)
        | Error::Connection(
            ConnectionError::TransportError(_)
            | ConnectionError::VersionMismatch
            | ConnectionError::CidsExhausted,
        )
        | Error::InvalidIdentity(_)
        | Error::SendDatagram(
            SendDatagramError::TooLarge
            | SendDatagramError::UnsupportedByPeer
            | SendDatagramError::Disabled,
        ) => add(&stats.connect_failed),
        Error::NotAdmitted(_) | Error::Banned(_) => add(&stats.handshake_rejected_unauthorized),
        Error::TableFull => add(&stats.handshake_rejected_overload),
        Error::IdentityRotated(_) => add(&stats.connection_evicted_identity_rotated),
        // Construction-time only; no runtime counter.
        Error::Endpoint(_) | Error::NoSockets => {}
    }
}

macro_rules! swap {
    ($m:expr) => {
        $m.swap(0, Ordering::Relaxed) as i64
    };
}

/// Re-baseline the peak high-water mark to the current occupancy and return
/// the peak observed over the period just ending.
#[inline]
fn take_peak(stats: &QuicDatagramStats, live_connections: u64) -> i64 {
    stats
        .peak_connections
        .swap(live_connections, Ordering::Relaxed)
        .max(live_connections) as i64
}

/// Emit and reset the outbound counters.
pub(crate) fn report_client(stats: &QuicDatagramStats, live_connections: u64) {
    datapoint_info!(
        "votor_datagram_client",
        ("connections_peak", take_peak(stats, live_connections), i64),
        ("datagrams_sent", swap!(stats.datagrams_sent), i64),
        ("connect_failed", swap!(stats.connect_failed), i64),
        ("connection_lost", swap!(stats.connection_lost), i64),
        (
            "egress_dropped_connect_in_progress",
            swap!(stats.egress_dropped_connect_in_progress),
            i64
        ),
        (
            "connection_evicted_peer_moved",
            swap!(stats.connection_evicted_peer_moved),
            i64
        ),
        (
            "connection_evicted_identity_rotated",
            swap!(stats.connection_evicted_identity_rotated),
            i64
        ),
    );
}

/// Emit and reset the inbound counters.
pub(crate) fn report_server(stats: &QuicDatagramStats, live_connections: u64) {
    datapoint_info!(
        "votor_datagram_server",
        ("connections_peak", take_peak(stats, live_connections), i64),
        ("datagrams_received", swap!(stats.datagrams_received), i64),
        ("handshakes_started", swap!(stats.handshakes_started), i64),
        (
            "handshakes_completed",
            swap!(stats.handshakes_completed),
            i64
        ),
        ("connect_failed", swap!(stats.connect_failed), i64),
        ("connection_lost", swap!(stats.connection_lost), i64),
        (
            "datagram_rate_limited",
            swap!(stats.datagram_rate_limited),
            i64
        ),
        (
            "datagram_ingress_dropped_channel_full",
            swap!(stats.datagram_ingress_dropped_channel_full),
            i64
        ),
        (
            "handshake_rejected_unauthorized",
            swap!(stats.handshake_rejected_unauthorized),
            i64
        ),
        (
            "handshake_rejected_overload",
            swap!(stats.handshake_rejected_overload),
            i64
        ),
        (
            "handshake_rate_limited",
            swap!(stats.handshake_rate_limited),
            i64
        ),
        ("handshake_timed_out", swap!(stats.handshake_timed_out), i64),
        (
            "connection_evicted_allowlist",
            swap!(stats.connection_evicted_allowlist),
            i64
        ),
        (
            "connection_evicted_identity_rotated",
            swap!(stats.connection_evicted_identity_rotated),
            i64
        ),
    );
}
