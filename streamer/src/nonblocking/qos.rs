use {
    crate::nonblocking::quic::{ClientConnectionTracker, ConnectionPeerType},
    quinn::Connection,
    std::{future::Future, time::Duration},
    tokio_util::sync::CancellationToken,
};

/// A trait to provide context about a connection, such as peer type,
/// remote pubkey. This is opaque to the framework and is provided by
/// the concrete implementation of QosController.
pub(crate) trait ConnectionContext: Clone + Send + Sync {
    fn peer_type(&self) -> ConnectionPeerType;
    fn remote_pubkey(&self) -> Option<solana_pubkey::Pubkey>;
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ParkedStreamMode {
    /// Park and periodically re-check saturation before accepting more streams.
    Park,
    /// Accept streams but immediately stop/reset them.
    Reset,
    /// Process streams as usual (subject to whatever credit is already issued).
    Allow,
}

/// A trait to manage QoS for connections. This includes
/// 1) deriving the ConnectionContext for a connection
/// 2) managing connection caching and connection limits, stream limits
pub(crate) trait QosController<C: ConnectionContext> {
    /// Build the ConnectionContext for a connection
    fn build_connection_context(&self, connection: &Connection) -> C;

    /// Try to add a new connection to the connection table. This is an async operation that
    /// returns a Future. If successful, the Future resolves to Some containing a CancellationToken.
    /// Otherwise, the Future resolves to None.
    fn try_add_connection(
        &self,
        client_connection_tracker: ClientConnectionTracker,
        connection: &quinn::Connection,
        context: &mut C,
    ) -> impl Future<Output = Option<CancellationToken>> + Send;

    /// Called when a new stream is received on a connection
    fn on_new_stream(&self, context: &C) -> impl Future<Output = ()> + Send;

    /// Called when a stream is accepted on a connection
    fn on_stream_accepted(&self, context: &C);

    /// Called when a stream is finished successfully
    fn on_stream_finished(&self, context: &C);

    /// Called when a stream has an error
    fn on_stream_error(&self, context: &C);

    /// Called when a stream is closed
    fn on_stream_closed(&self, context: &C);

    /// Remove a connection. Return the number of open connections after removal.
    fn remove_connection(
        &self,
        context: &C,
        connection: Connection,
    ) -> impl Future<Output = usize> + Send;

    /// Whether the system is globally saturated. Called at the top of the
    /// connection loop and passed to [`compute_max_streams`].
    fn is_saturated(&self) -> bool {
        false
    }

    /// Returns the desired max concurrent uni streams for a connection.
    /// - `Some(n)` — set max_concurrent_uni_streams to n. If n == 0, park the connection.
    /// - `None` — don't change MAX_STREAMS (let the QoS use on_new_stream for throttling).
    fn compute_max_streams(
        &self,
        context: &C,
        connection: &Connection,
        saturated: bool,
    ) -> Option<u32> {
        let _ = (context, connection, saturated);
        None
    }

    /// Behavior for connections that are effectively parked (MAX_STREAMS == 0).
    fn parked_stream_mode(&self, _context: &C) -> ParkedStreamMode {
        ParkedStreamMode::Park
    }

    /// Pull QoS-specific metrics into StreamerStats before reporting.
    /// `elapsed` is the time since the last pull.
    fn pull_stats(&self, _stats: &crate::quic::StreamerStats, _elapsed: Duration) {}

    /// How many concurrent
    fn max_concurrent_connections(&self) -> usize;
}

/// Marker trait to indicate what is the shared state for connections
pub(crate) trait OpaqueStreamerCounter: Send + Sync + 'static {}

#[cfg(test)]
pub(crate) struct NullStreamerCounter;

#[cfg(test)]
impl OpaqueStreamerCounter for NullStreamerCounter {}
