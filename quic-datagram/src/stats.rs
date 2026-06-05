use {
    crate::error::Error,
    quinn::{ConnectionError, SendDatagramError},
    solana_metrics::datapoint_info,
    std::sync::atomic::{AtomicU64, Ordering},
};

/// Counters reported every metrics tick (cadence owned by the control loop)
/// under the `votor_datagram` namespace.
///
/// Error variants are collapsed into coarse buckets: the exact variant is
/// rarely actionable on its own and is still emitted to the log by
/// `record_error`'s callers.
#[derive(Default)]
pub(crate) struct QuicDatagramStats {
    // --- Positive path ---
    /// High-water mark of live table entries over the reporting period.
    /// Updated via `fetch_max` on every successful insert; re-baselined to the
    /// current occupancy at each report.
    pub(crate) peak_connections: AtomicU64,
    /// Datagrams successfully delivered to the ingress channel.
    pub(crate) datagrams_received: AtomicU64,
    /// Datagrams successfully handed to quinn for transmission.
    pub(crate) datagrams_sent: AtomicU64,
    /// A live connection ended through an expected teardown path
    /// (application/connection close, local close, reset, timeout), or a
    /// `send_datagram` found the connection already gone. This is normal
    /// churn, not likely to be a fault.
    pub(crate) connection_lost: AtomicU64,
    /// Egress dropped because local_pubkey >= peer (we are the lex-higher
    /// side, so we never dial) and no inbound connection from that peer
    /// has been accepted yet.
    pub(crate) egress_dropped_higher_pubkey: AtomicU64,
    /// Follower egress dropped because a dial is already in flight for the
    /// peer. The egress that *triggered* the dial is carried into the dial
    /// task and sent the moment the connection lands; only subsequent egress
    /// arriving while the slot still holds a `Dialing` placeholder is dropped.
    pub(crate) egress_dropped_dial_in_progress: AtomicU64,

    // --- Connection lifecycle management ---
    /// Connections closed because the local identity (TLS cert / pubkey) was
    /// rotated. Each rotation evicts every cached connection in one burst.
    pub(crate) connection_evicted_identity_rotated: AtomicU64,
    /// Cached connection closed at insert-cap time to make room for a
    /// freshly-allowed peer that displaced a no-longer-allowed one.
    pub(crate) connection_evicted_allowlist: AtomicU64,
    /// Lex-lower side evicted a cached outbound connection because the caller
    /// supplied a new socket addr for the same pubkey (peer moved).
    pub(crate) connection_evicted_peer_moved: AtomicU64,
    /// We closed a peer's existing connection with HANDOVER because a new
    /// handshake from the same pubkey arrived.
    pub(crate) connection_replaced_handover: AtomicU64,
    /// A peer closed our connection with HANDOVER - we have been replaced.
    pub(crate) handover_received: AtomicU64,

    // --- Error buckets ---
    /// A connection failed abnormally: dial setup error, protocol-level
    /// connection fault, or a non-transient `send_datagram` failure. All are
    /// local/config/protocol faults that should not occur in prod for a
    /// routable, correctly-built peer.
    pub(crate) connect_failed: AtomicU64,
    /// Handshake refused because the peer is not permitted: not in the
    /// allowlist, or currently banned.
    pub(crate) handshake_rejected_unauthorized: AtomicU64,
    /// Handshake refused because the peer misbehaved at the protocol level:
    /// unrecoverable identity, or wrong dial direction (lex tiebreaker).
    pub(crate) handshake_rejected_protocol: AtomicU64,
    /// Handshake refused due to a resource limit: connection table full, or
    /// the source subnet exceeded its handshake-attempt budget.
    pub(crate) handshake_rejected_overload: AtomicU64,
    /// Peer's incoming datagram exceeded the per-connection rate.
    pub(crate) datagram_rate_limited: AtomicU64,

    // --- Drops on internal channles (distinct backpressure signals) ---
    /// We have received a datagram but have nowhere to put it
    pub(crate) datagram_ingress_dropped_channel_full: AtomicU64,

    // --- DOS management ---
    /// RETRY tokens sent to unvalidated incoming addresses.
    pub(crate) handshake_retry_sent: AtomicU64,
}

#[inline]
pub(crate) fn add(metric: &AtomicU64) {
    metric.fetch_add(1, Ordering::Relaxed);
}

impl QuicDatagramStats {
    /// Updates the peak-occupancy high-water mark after a successful connection
    /// install.
    pub(crate) fn record_connection_count(&self, table_len: u64) {
        self.peak_connections
            .fetch_max(table_len, Ordering::Relaxed);
    }
}

pub(crate) fn record_error(err: &Error, stats: &QuicDatagramStats) {
    match err {
        Error::EgressChannelClosed | Error::IngressChannelClosed => {}
        Error::Connection(
            ConnectionError::ApplicationClosed(_)
            | ConnectionError::ConnectionClosed(_)
            | ConnectionError::LocallyClosed
            | ConnectionError::Reset
            | ConnectionError::TimedOut,
        )
        // A send that fails because the connection vanished is the same
        // class of event as a connection closing under us.
        | Error::SendDatagram(SendDatagramError::ConnectionLost(_)) => {
            add(&stats.connection_lost)
        }
        Error::Connect(_)
        | Error::Connection(
            ConnectionError::TransportError(_)
            | ConnectionError::VersionMismatch
            | ConnectionError::CidsExhausted,
        )
        // TooLarge / UnsupportedByPeer / Disabled: config/protocol faults,
        // same class as the dial-setup and protocol bucket above.
        | Error::SendDatagram(
            SendDatagramError::TooLarge
            | SendDatagramError::UnsupportedByPeer
            | SendDatagramError::Disabled,
        ) => add(&stats.connect_failed),
        Error::NotAdmitted(_) | Error::Banned(_) => add(&stats.handshake_rejected_unauthorized),
        Error::InvalidIdentity(_) | Error::WrongDirection(_) => {
            add(&stats.handshake_rejected_protocol)
        }
        Error::TableFull => add(&stats.handshake_rejected_overload),
        Error::IdentityRotated(_) => {
            // The new connection would have been an Established under the
            // rotated identity, so it counts as a rotation-driven eviction
            // (just preempted before it landed).
            add(&stats.connection_evicted_identity_rotated)
        }
        Error::Endpoint(_) => {} // construction-time only; no runtime counter
    }
}

/// Emit the accumulated counters and reset them. `live_connections` is the
/// current table occupancy, sampled by the control loop; it is NOT reported on
/// its own (a single sample of a churning table at the 2s tick is noise), but
/// is used to re-baseline the peak high-water mark for the next period so a
/// steady-state population still reports its size.
pub(crate) fn report(stats: &QuicDatagramStats, live_connections: u64) {
    macro_rules! swap {
        ($m:expr) => {
            $m.swap(0, Ordering::Relaxed) as i64
        };
    }
    // Re-baseline the peak to the current occupancy: the next period's peak
    // starts from where this one left off, not from zero.
    let peak_connections = stats
        .peak_connections
        .swap(live_connections, Ordering::Relaxed)
        .max(live_connections) as i64;
    datapoint_info!(
        "votor_datagram",
        // Positive path
        ("connections_peak", peak_connections, i64),
        ("datagrams_received", swap!(stats.datagrams_received), i64),
        ("datagrams_sent", swap!(stats.datagrams_sent), i64),
        // Error buckets
        ("connect_failed", swap!(stats.connect_failed), i64),
        ("connection_lost", swap!(stats.connection_lost), i64),
        (
            "handshake_rejected_unauthorized",
            swap!(stats.handshake_rejected_unauthorized),
            i64
        ),
        (
            "handshake_rejected_protocol",
            swap!(stats.handshake_rejected_protocol),
            i64
        ),
        (
            "handshake_rejected_overload",
            swap!(stats.handshake_rejected_overload),
            i64
        ),
        // Drops
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
            "egress_dropped_higher_pubkey",
            swap!(stats.egress_dropped_higher_pubkey),
            i64
        ),
        (
            "egress_dropped_dial_in_progress",
            swap!(stats.egress_dropped_dial_in_progress),
            i64
        ),
        // Lifecycle
        (
            "handshake_retry_sent",
            swap!(stats.handshake_retry_sent),
            i64
        ),
        (
            "connection_evicted_identity_rotated",
            swap!(stats.connection_evicted_identity_rotated),
            i64
        ),
        (
            "connection_evicted_allowlist",
            swap!(stats.connection_evicted_allowlist),
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
    );
}
