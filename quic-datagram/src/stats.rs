use {
    crate::error::Error,
    quinn::{ConnectionError, SendDatagramError},
    solana_metrics::datapoint_info,
    std::sync::atomic::{AtomicU64, Ordering},
};

/// Counters for the outbound (we-connect, send-only) direction.
#[derive(Default)]
pub struct ClientStats {
    /// High-water mark of live table entries over the reporting period.
    pub(crate) peak_connections: AtomicU64,
    /// Datagrams successfully handed to quinn for transmission.
    pub(crate) datagrams_sent: AtomicU64,
    /// A connection ended through an expected teardown path.
    pub(crate) connection_lost: AtomicU64,
    /// Egress dropped because the connection for the peer is not ready.
    pub(crate) egress_dropped_connect_in_progress: AtomicU64,
    /// A connection failed abnormally: connect setup error, protocol-level
    /// connection fault, or a non-transient `send_datagram` failure.
    pub(crate) connect_failed: AtomicU64,
    /// Connections closed because the local identity (TLS cert / pubkey) was
    /// rotated.
    pub(crate) connection_evicted_identity_rotated: AtomicU64,
    /// Existing connection closed because the caller supplied
    /// a new address for the same pubkey.
    pub(crate) connection_evicted_peer_moved: AtomicU64,
}

/// Counters for the inbound (we-accept, receive-only) direction.
#[derive(Default)]
pub struct ServerStats {
    /// High-water mark of live table entries over the reporting period.
    pub(crate) peak_connections: AtomicU64,
    /// Datagrams successfully delivered to the ingress channel.
    pub(crate) datagrams_received: AtomicU64,
    /// A connection ended through an expected teardown path.
    pub(crate) connection_lost: AtomicU64,
    /// A connection failed abnormally during accept, handshake, or read.
    pub(crate) connection_failed: AtomicU64,
    /// Inbound handshakes we began TLS work for.
    pub(crate) handshakes_started: AtomicU64,
    /// Inbound TLS handshakes that completed successfully and yielded an
    /// authenticated connection.
    pub(crate) handshakes_completed: AtomicU64,
    /// Handshake refused because the peer is not permitted: not in the
    /// allowlist, or currently banned.
    pub(crate) handshake_rejected_unauthorized: AtomicU64,
    /// Handshake refused due to a resource limit: connection table full.
    pub(crate) handshake_rejected_overload: AtomicU64,
    /// Number of times the accept gate closed because the global handshake rate
    /// limit was exhausted, pausing how fast we pull new attempts off the
    /// endpoint.
    pub(crate) handshake_rate_limited: AtomicU64,
    /// Handshakes that did not complete within `HANDSHAKE_TIMEOUT`.
    pub(crate) handshake_timed_out: AtomicU64,
    /// Connections closed because the peer is no longer
    /// admitted (e.g. evicted at an epoch boundary).
    pub(crate) connection_evicted_allowlist: AtomicU64,
    /// Connections closed because the peer was banned by the sig-verifier.
    pub(crate) connection_evicted_banned: AtomicU64,
    /// Connections closed because the local identity (TLS cert / pubkey) was
    /// rotated.
    pub(crate) connection_evicted_identity_rotated: AtomicU64,
    /// We have received a datagram but have nowhere to put it.
    pub(crate) datagram_ingress_dropped_channel_full: AtomicU64,
    /// Peer's incoming datagram exceeded the per-connection rate.
    pub(crate) datagram_rate_limited: AtomicU64,
}

#[inline]
pub(crate) fn add(metric: &AtomicU64) {
    metric.fetch_add(1, Ordering::Relaxed);
}

/// Raise a peak-occupancy high-water mark to `count` if it is higher.
pub(crate) fn record_connection_count(peak: &AtomicU64, count: u64) {
    peak.fetch_max(count, Ordering::Relaxed);
}

/// Route an outbound-direction error into the client counters.
pub(crate) fn record_client_error(err: &Error, stats: &ClientStats) {
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
        // The outbound direction never produces admission, ban, table-full,
        // rotation, or construction-time errors.
        Error::NotAdmitted(_)
        | Error::Banned(_)
        | Error::TableFull
        | Error::IdentityRotated(_)
        | Error::Endpoint(_)
        | Error::NoSockets => unreachable!("outbound direction does not produce {err:?}"),
    }
}

/// Route an inbound-direction error into the server counters.
pub(crate) fn record_server_error(err: &Error, stats: &ServerStats) {
    match err {
        Error::Connection(
            ConnectionError::ApplicationClosed(_)
            | ConnectionError::ConnectionClosed(_)
            | ConnectionError::LocallyClosed
            | ConnectionError::Reset
            | ConnectionError::TimedOut,
        ) => add(&stats.connection_lost),
        Error::Connection(
            ConnectionError::TransportError(_)
            | ConnectionError::VersionMismatch
            | ConnectionError::CidsExhausted,
        )
        | Error::InvalidIdentity(_) => add(&stats.connection_failed),
        Error::NotAdmitted(_) | Error::Banned(_) => add(&stats.handshake_rejected_unauthorized),
        Error::TableFull => add(&stats.handshake_rejected_overload),
        // The inbound direction never dials, is receive-only, and records
        // rotation evictions directly, so it never routes these through here.
        Error::Connect(_)
        | Error::SendDatagram(_)
        | Error::IdentityRotated(_)
        | Error::Endpoint(_)
        | Error::NoSockets => unreachable!("inbound direction does not produce {err:?}"),
    }
}

macro_rules! swap {
    ($m:expr) => {
        $m.swap(0, Ordering::Relaxed) as i64
    };
}

/// Re-baseline the peak high-water mark to the current occupancy and return the
/// peak observed over the period just ending.
#[inline]
fn take_peak(peak: &AtomicU64, live_connections: u64) -> i64 {
    peak.swap(live_connections, Ordering::Relaxed)
        .max(live_connections) as i64
}

/// Emit and reset the outbound counters.
pub(crate) fn report_client(stats: &ClientStats, live_connections: u64) {
    datapoint_info!(
        "votor_datagram_client",
        (
            "connections_peak",
            take_peak(&stats.peak_connections, live_connections),
            i64
        ),
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
pub(crate) fn report_server(stats: &ServerStats, live_connections: u64) {
    datapoint_info!(
        "votor_datagram_server",
        (
            "connections_peak",
            take_peak(&stats.peak_connections, live_connections),
            i64
        ),
        ("datagrams_received", swap!(stats.datagrams_received), i64),
        ("handshakes_started", swap!(stats.handshakes_started), i64),
        (
            "handshakes_completed",
            swap!(stats.handshakes_completed),
            i64
        ),
        ("connect_failed", swap!(stats.connection_failed), i64),
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
            "connection_evicted_banned",
            swap!(stats.connection_evicted_banned),
            i64
        ),
        (
            "connection_evicted_identity_rotated",
            swap!(stats.connection_evicted_identity_rotated),
            i64
        ),
    );
}
