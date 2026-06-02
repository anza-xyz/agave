use {
    crate::error::Error,
    quinn::{ConnectError, ConnectionError, SendDatagramError},
    solana_metrics::datapoint_info,
    std::sync::atomic::{AtomicU64, Ordering},
};

/// Counters reported every metrics tick (cadence owned by the control loop)
/// under the `votor_datagram` namespace.
#[derive(Default)]
pub(crate) struct QuicDatagramStats {
    // Connect-side (client) failures
    pub(crate) connect_error_endpoint_stopping: AtomicU64,
    pub(crate) connect_error_invalid_remote_address: AtomicU64,
    pub(crate) connect_error_unsupported_version: AtomicU64,
    pub(crate) connect_error_no_default_client_config: AtomicU64,
    pub(crate) connect_error_cids_exhausted: AtomicU64,
    pub(crate) connect_error_invalid_server_name: AtomicU64,

    // Connection-level errors
    pub(crate) connection_error_application_closed: AtomicU64,
    pub(crate) connection_error_connection_closed: AtomicU64,
    pub(crate) connection_error_locally_closed: AtomicU64,
    pub(crate) connection_error_reset: AtomicU64,
    pub(crate) connection_error_timed_out: AtomicU64,
    pub(crate) connection_error_transport: AtomicU64,
    pub(crate) connection_error_version_mismatch: AtomicU64,
    pub(crate) connection_error_cids_exhausted: AtomicU64,

    // Identity / admission
    pub(crate) invalid_identity: AtomicU64,
    pub(crate) handshake_rejected_not_admitted: AtomicU64,
    pub(crate) handshake_rejected_banned: AtomicU64,
    pub(crate) handshake_rejected_table_full: AtomicU64,
    pub(crate) handshake_rejected_wrong_direction: AtomicU64,
    /// Source IP's subnet exceeded its handshake-attempt budget
    pub(crate) handshake_rejected_subnet_flood: AtomicU64,
    pub(crate) handshake_retry_sent: AtomicU64,

    // Datagram path
    pub(crate) datagram_ingress_dropped_channel_full: AtomicU64,
    /// Peer's incoming datagram exceeded the per-connection rate.
    pub(crate) datagram_rate_limited: AtomicU64,
    pub(crate) send_datagram_error_connection_lost: AtomicU64,
    pub(crate) send_datagram_error_too_large: AtomicU64,
    pub(crate) send_datagram_error_unsupported_by_peer: AtomicU64,
    pub(crate) send_datagram_error_disabled: AtomicU64,
    /// Egress dropped because local_pubkey >= peer (we are the lex-higher
    /// side, so we never dial) and no inbound connection from that peer
    /// has been accepted yet.
    pub(crate) egress_dropped_higher_pubkey: AtomicU64,
    /// Follower egress dropped because a dial is already in flight for
    /// the peer. The egress that *triggered* the dial is carried into
    /// the dial task and sent the moment the connection lands; only
    /// subsequent egress arriving while the slot still holds a `Dialing`
    /// placeholder is dropped (we do not buffer per-peer datagrams). The
    /// next egress after the dial resolves uses the now-`Established`
    /// connection.
    pub(crate) egress_dropped_dial_in_progress: AtomicU64,

    // Table evictions
    /// Connections closed because the local identity (TLS cert / pubkey)
    /// was rotated. Each rotation evicts every cached connection in one
    /// burst; peers re-handshake against the new cert on their next send.
    pub(crate) connection_evicted_identity_rotated: AtomicU64,
    /// Cached connection closed at insert-cap time to make room for a
    /// freshly-admitted peer. The displaced peer was no longer in the
    /// admission set - typically an epoch-boundary unstaked-out validator
    /// being displaced by a newly-staked one. Closed with `NOT_ADMITTED`.
    pub(crate) connection_evicted_admission: AtomicU64,
    /// Lex-lower side evicted a cached outbound connection because the
    /// caller supplied a new socket addr for the same pubkey (e.g. gossip
    /// observed the peer moved). A re-dial to the new addr follows.
    pub(crate) connection_evicted_peer_moved: AtomicU64,
    /// We closed a peer's existing connection with HANDOVER because a new
    /// handshake from the same pubkey arrived.
    pub(crate) connection_replaced_handover: AtomicU64,
    /// A peer closed our connection with HANDOVER - we have been replaced
    /// (typically by a backup-node instance of our own identity).
    pub(crate) handover_received: AtomicU64,
}

#[inline]
pub(crate) fn add(metric: &AtomicU64) {
    metric.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_error(err: &Error, stats: &QuicDatagramStats) {
    match err {
        Error::EgressChannelClosed | Error::IngressChannelClosed => {}
        Error::Connect(ConnectError::EndpointStopping) => {
            add(&stats.connect_error_endpoint_stopping)
        }
        Error::Connect(ConnectError::InvalidRemoteAddress(_)) => {
            add(&stats.connect_error_invalid_remote_address)
        }
        Error::Connect(ConnectError::UnsupportedVersion) => {
            add(&stats.connect_error_unsupported_version)
        }
        Error::Connect(ConnectError::NoDefaultClientConfig) => {
            add(&stats.connect_error_no_default_client_config)
        }
        Error::Connect(ConnectError::CidsExhausted) => add(&stats.connect_error_cids_exhausted),
        Error::Connect(ConnectError::InvalidServerName(_)) => {
            add(&stats.connect_error_invalid_server_name)
        }
        Error::Connection(ConnectionError::ApplicationClosed(_)) => {
            add(&stats.connection_error_application_closed)
        }
        Error::Connection(ConnectionError::ConnectionClosed(_)) => {
            add(&stats.connection_error_connection_closed)
        }
        Error::Connection(ConnectionError::LocallyClosed) => {
            add(&stats.connection_error_locally_closed)
        }
        Error::Connection(ConnectionError::Reset) => add(&stats.connection_error_reset),
        Error::Connection(ConnectionError::TimedOut) => add(&stats.connection_error_timed_out),
        Error::Connection(ConnectionError::TransportError(_)) => {
            add(&stats.connection_error_transport)
        }
        Error::Connection(ConnectionError::VersionMismatch) => {
            add(&stats.connection_error_version_mismatch)
        }
        Error::Connection(ConnectionError::CidsExhausted) => {
            add(&stats.connection_error_cids_exhausted)
        }
        Error::InvalidIdentity(_) => add(&stats.invalid_identity),
        Error::NotAdmitted(_) => add(&stats.handshake_rejected_not_admitted),
        Error::Banned(_) => add(&stats.handshake_rejected_banned),
        Error::TableFull => add(&stats.handshake_rejected_table_full),
        Error::WrongDirection(_) => add(&stats.handshake_rejected_wrong_direction),
        Error::SendDatagram(SendDatagramError::ConnectionLost(_)) => {
            add(&stats.send_datagram_error_connection_lost)
        }
        Error::SendDatagram(SendDatagramError::TooLarge) => {
            add(&stats.send_datagram_error_too_large)
        }
        Error::SendDatagram(SendDatagramError::UnsupportedByPeer) => {
            add(&stats.send_datagram_error_unsupported_by_peer)
        }
        Error::SendDatagram(SendDatagramError::Disabled) => {
            add(&stats.send_datagram_error_disabled)
        }
        Error::IdentityRotated(_) => {
            // Reuse the rotation counter: the new connection would have
            // been an Established under the rotated identity, so it
            // counts as a rotation-driven eviction (just preempted
            // before it landed).
            add(&stats.connection_evicted_identity_rotated)
        }
        Error::Endpoint(_) => {} // construction-time only; no runtime counter
    }
}

pub(crate) fn report(stats: &QuicDatagramStats) {
    macro_rules! swap {
        ($m:expr) => {
            $m.swap(0, Ordering::Relaxed) as i64
        };
    }
    datapoint_info!(
        "votor_datagram",
        (
            "connect_error_endpoint_stopping",
            swap!(stats.connect_error_endpoint_stopping),
            i64
        ),
        (
            "connect_error_invalid_remote_address",
            swap!(stats.connect_error_invalid_remote_address),
            i64
        ),
        (
            "connect_error_unsupported_version",
            swap!(stats.connect_error_unsupported_version),
            i64
        ),
        (
            "connect_error_no_default_client_config",
            swap!(stats.connect_error_no_default_client_config),
            i64
        ),
        (
            "connect_error_cids_exhausted",
            swap!(stats.connect_error_cids_exhausted),
            i64
        ),
        (
            "connect_error_invalid_server_name",
            swap!(stats.connect_error_invalid_server_name),
            i64
        ),
        (
            "connection_error_application_closed",
            swap!(stats.connection_error_application_closed),
            i64
        ),
        (
            "connection_error_connection_closed",
            swap!(stats.connection_error_connection_closed),
            i64
        ),
        (
            "connection_error_locally_closed",
            swap!(stats.connection_error_locally_closed),
            i64
        ),
        (
            "connection_error_reset",
            swap!(stats.connection_error_reset),
            i64
        ),
        (
            "connection_error_timed_out",
            swap!(stats.connection_error_timed_out),
            i64
        ),
        (
            "connection_error_transport",
            swap!(stats.connection_error_transport),
            i64
        ),
        (
            "connection_error_version_mismatch",
            swap!(stats.connection_error_version_mismatch),
            i64
        ),
        (
            "connection_error_cids_exhausted",
            swap!(stats.connection_error_cids_exhausted),
            i64
        ),
        ("invalid_identity", swap!(stats.invalid_identity), i64),
        (
            "handshake_rejected_not_admitted",
            swap!(stats.handshake_rejected_not_admitted),
            i64
        ),
        (
            "handshake_rejected_banned",
            swap!(stats.handshake_rejected_banned),
            i64
        ),
        (
            "handshake_rejected_table_full",
            swap!(stats.handshake_rejected_table_full),
            i64
        ),
        (
            "handshake_rejected_wrong_direction",
            swap!(stats.handshake_rejected_wrong_direction),
            i64
        ),
        (
            "handshake_rejected_subnet_flood",
            swap!(stats.handshake_rejected_subnet_flood),
            i64
        ),
        (
            "handshake_retry_sent",
            swap!(stats.handshake_retry_sent),
            i64
        ),
        (
            "datagram_ingress_dropped_channel_full",
            swap!(stats.datagram_ingress_dropped_channel_full),
            i64
        ),
        (
            "datagram_rate_limited",
            swap!(stats.datagram_rate_limited),
            i64
        ),
        (
            "send_datagram_error_connection_lost",
            swap!(stats.send_datagram_error_connection_lost),
            i64
        ),
        (
            "send_datagram_error_too_large",
            swap!(stats.send_datagram_error_too_large),
            i64
        ),
        (
            "send_datagram_error_unsupported_by_peer",
            swap!(stats.send_datagram_error_unsupported_by_peer),
            i64
        ),
        (
            "send_datagram_error_disabled",
            swap!(stats.send_datagram_error_disabled),
            i64
        ),
        (
            "connection_evicted_identity_rotated",
            swap!(stats.connection_evicted_identity_rotated),
            i64
        ),
        (
            "connection_evicted_admission",
            swap!(stats.connection_evicted_admission),
            i64
        ),
        (
            "connection_evicted_peer_moved",
            swap!(stats.connection_evicted_peer_moved),
            i64
        ),
        (
            "connection_replaced_handover",
            swap!(stats.connection_replaced_handover),
            i64
        ),
        ("handover_received", swap!(stats.handover_received), i64),
        (
            "egress_dropped_higher_pubkey",
            swap!(stats.egress_dropped_higher_pubkey),
            i64
        ),
        (
            "egress_dropped_dial_in_progress",
            swap!(stats.egress_dropped_dial_in_progress),
            i64
        ),
    );
}
