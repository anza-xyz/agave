#[cfg(test)]
use std::sync::atomic::AtomicBool;
use {
    crate::error::Error,
    quinn::{ConnectionError, SendDatagramError},
    solana_metrics::datapoint_info,
    std::sync::atomic::{AtomicU64, Ordering},
};

/// Counters for the outbound (we-connect, send-only) direction.
#[derive(Default)]
pub struct ClientStats {
    /// High-water mark of live connections over the reporting period.
    pub(crate) peak_connections: AtomicU64,
    /// Datagrams successfully handed to quinn for transmission.
    pub(crate) datagrams_sent: AtomicU64,
    /// A connection ended through an expected teardown path.
    pub(crate) connection_lost: AtomicU64,
    /// A connection failed abnormally: connect setup error, protocol-level
    /// connection fault, or a non-transient `send_datagram` failure.
    pub(crate) connect_failed: AtomicU64,
    /// A peer in the peer_list had no resolvable address.
    pub(crate) connect_failed_no_address: AtomicU64,
    /// Connections closed because the local identity changed.
    pub(crate) connection_closed_identity_changed: AtomicU64,
    /// Existing connection closed because the peer's gossip address changed.
    pub(crate) connection_closed_peer_moved: AtomicU64,
    /// Connections closed because the peer is no longer in the peer_list.
    pub(crate) connection_closed_not_in_peer_list: AtomicU64,
}

/// Counters for the inbound (we-accept, receive-only) direction.
#[derive(Default)]
pub struct ServerStats {
    /// High-water mark of live table entries over the reporting period.
    pub(crate) peak_unique_peers: AtomicU64,
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
    /// peer_list, or currently banned.
    pub handshake_rejected_unauthorized: AtomicU64,
    /// Handshake refused due to a resource limit: connection table full.
    pub(crate) handshake_rejected_overload: AtomicU64,
    /// Inbound attempts shed because the global handshake rate limit was
    /// exhausted (and the accept gate was closed until it refills).
    pub(crate) handshake_rate_limited: AtomicU64,
    /// Handshakes that did not complete within `HANDSHAKE_TIMEOUT`.
    pub(crate) handshake_timed_out: AtomicU64,
    /// Connections closed because the peer is no longer
    /// admitted (e.g. lost stake at an epoch boundary).
    pub(crate) connection_closed_not_in_peer_list: AtomicU64,
    /// Connections closed because the peer was banned by the sig-verifier.
    pub connection_closed_banned: AtomicU64,
    /// Connections closed because the local identity changed.
    pub(crate) connection_closed_identity_changed: AtomicU64,
    /// We have received a datagram but have nowhere to put it.
    pub(crate) datagram_ingress_dropped_channel_full: AtomicU64,
    /// Peer's incoming datagram exceeded the per-connection rate.
    pub(crate) datagram_rate_limited: AtomicU64,
    /// When set, `report_server` skips its periodic emit-and-reset,
    /// so counters stay cumulative for assertions that need
    /// totals across reporting ticks.
    #[cfg(test)]
    pub(crate) report_frozen: AtomicBool,
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
        Error::NotAdmitted(_)
        | Error::Banned(_)
        | Error::TooManyConnections
        | Error::Endpoint(_) => {
            debug_assert!(false, "outbound direction does not produce {err:?}");
        }
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
        Error::TooManyConnections => add(&stats.handshake_rejected_overload),
        Error::Connect(_) | Error::SendDatagram(_) | Error::Endpoint(_) => {
            debug_assert!(false, "inbound direction does not produce {err:?}");
        }
    }
}

macro_rules! swap {
    ($m:expr) => {
        $m.swap(0, Ordering::Relaxed) as i64
    };
}

/// Re-baseline the peak to the current value and return the
/// peak observed over the period just ending.
#[inline]
fn take_peak(peak: &AtomicU64, live: u64) -> i64 {
    peak.swap(live, Ordering::Relaxed).max(live) as i64
}

/// Emit and reset the outbound counters.
pub(crate) fn report_client(stats: &ClientStats, live_connections: u64) {
    // Snapshot and reset every counter unconditionally, *before* `datapoint_info!`
    // so we do not end up with huge values here when operator changes log level.
    let connections_peak = take_peak(&stats.peak_connections, live_connections);
    let datagrams_sent = swap!(stats.datagrams_sent);
    let connect_failed = swap!(stats.connect_failed);
    let connect_failed_no_address = swap!(stats.connect_failed_no_address);
    let connection_lost = swap!(stats.connection_lost);
    let connection_closed_peer_moved = swap!(stats.connection_closed_peer_moved);
    let connection_closed_not_in_peer_list = swap!(stats.connection_closed_not_in_peer_list);
    let connection_closed_identity_changed = swap!(stats.connection_closed_identity_changed);
    datapoint_info!(
        "votor_datagram_client",
        ("connections_peak", connections_peak, i64),
        ("datagrams_sent", datagrams_sent, i64),
        ("connect_failed", connect_failed, i64),
        ("connect_failed_no_address", connect_failed_no_address, i64),
        ("connection_lost", connection_lost, i64),
        (
            "connection_closed_peer_moved",
            connection_closed_peer_moved,
            i64
        ),
        (
            "connection_closed_not_in_peer_list",
            connection_closed_not_in_peer_list,
            i64
        ),
        (
            "connection_closed_identity_changed",
            connection_closed_identity_changed,
            i64
        ),
    );
}

/// Emit and reset the inbound counters.
pub(crate) fn report_server(stats: &ServerStats, live_connections: u64) {
    #[cfg(test)]
    if stats.report_frozen.load(Ordering::Relaxed) {
        // Leave counters untouched so test assertions can read
        // cumulative totals across reporting ticks.
        return;
    }
    // Snapshot-and-reset every counter unconditionally, *before* `datapoint_info!`.
    let unique_peers_peak = take_peak(&stats.peak_unique_peers, live_connections);
    let datagrams_received = swap!(stats.datagrams_received);
    let handshakes_started = swap!(stats.handshakes_started);
    let handshakes_completed = swap!(stats.handshakes_completed);
    let connection_failed = swap!(stats.connection_failed);
    let connection_lost = swap!(stats.connection_lost);
    let datagram_rate_limited = swap!(stats.datagram_rate_limited);
    let datagram_ingress_dropped_channel_full = swap!(stats.datagram_ingress_dropped_channel_full);
    let handshake_rejected_unauthorized = swap!(stats.handshake_rejected_unauthorized);
    let handshake_rejected_overload = swap!(stats.handshake_rejected_overload);
    let handshake_rate_limited = swap!(stats.handshake_rate_limited);
    let handshake_timed_out = swap!(stats.handshake_timed_out);
    let connection_closed_not_in_peer_list = swap!(stats.connection_closed_not_in_peer_list);
    let connection_closed_banned = swap!(stats.connection_closed_banned);
    let connection_closed_identity_changed = swap!(stats.connection_closed_identity_changed);
    datapoint_info!(
        "votor_datagram_server",
        ("unique_peers_peak", unique_peers_peak, i64),
        ("datagrams_received", datagrams_received, i64),
        ("handshakes_started", handshakes_started, i64),
        ("handshakes_completed", handshakes_completed, i64),
        ("connection_failed", connection_failed, i64),
        ("connection_lost", connection_lost, i64),
        ("datagram_rate_limited", datagram_rate_limited, i64),
        (
            "datagram_ingress_dropped_channel_full",
            datagram_ingress_dropped_channel_full,
            i64
        ),
        (
            "handshake_rejected_unauthorized",
            handshake_rejected_unauthorized,
            i64
        ),
        (
            "handshake_rejected_overload",
            handshake_rejected_overload,
            i64
        ),
        ("handshake_rate_limited", handshake_rate_limited, i64),
        ("handshake_timed_out", handshake_timed_out, i64),
        (
            "connection_closed_not_in_peer_list",
            connection_closed_not_in_peer_list,
            i64
        ),
        ("connection_closed_banned", connection_closed_banned, i64),
        (
            "connection_closed_identity_changed",
            connection_closed_identity_changed,
            i64
        ),
    );
}
