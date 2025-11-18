use {
    crate::nonblocking::quic::{ClientConnectionTracker, ConnectionPeerType},
    quinn::Connection,
    std::future::Future,
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

    /// How many concurrent
    fn max_concurrent_connections(&self) -> usize;
}

pub trait QosConfig {}
