use {
    crate::nonblocking::{
        quic::{ClientConnectionTracker, ConnectionPeerType},
        stream_throttle::ConnectionStreamCounter,
    },
    quinn::Connection,
    std::{sync::Arc, time::Duration},
    tokio_util::sync::CancellationToken,
};

/// A trait to provide context about a connection, such as peer type,
/// remote pubkey. This is opaque to the framework and is provided by
/// the concrete implementation of QosController.
pub(crate) trait ConnectionContext: Clone + Send + Sync {
    fn peer_type(&self) -> ConnectionPeerType;
    fn remote_pubkey(&self) -> Option<solana_pubkey::Pubkey>;
}

/// A trait to manage QoS for connections. This includes
/// 1) deriving the ConnectionContext for a connection
/// 2) managing connect caching and connection limits
pub(crate) trait QosController<C: ConnectionContext> {
    /// Derive the ConnectionContext for a connection
    fn derive_connection_context(&self, connection: &Connection) -> C;

    /// Try to add a new connection to cache. If successful, return a CancellationToken and
    /// a ConnectionStreamCounter to track the streams created on this connection.
    /// Otherwise return None.
    fn try_cache_connection(
        &self,
        client_connection_tracker: ClientConnectionTracker,
        connection: &quinn::Connection,
        context: &mut C,
    ) -> impl std::future::Future<Output = Option<(CancellationToken, Arc<ConnectionStreamCounter>)>>
           + Send;

    /// The maximum number of streams that can be opened per throttling interval
    /// on this connection.
    fn max_streams_per_throttling_interval(&self, context: &C) -> u64;

    fn total_stake(&self) -> u64;

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
    ) -> impl std::future::Future<Output = usize> + Send;

    /// The timeout duration to wait for a chunk to arrive on a stream
    fn wait_for_chunk_timeout(&self) -> Duration;
}
