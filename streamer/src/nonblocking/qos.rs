use {
    crate::{
        nonblocking::quic::{ClientConnectionTracker, ConnectionPeerType, get_pubkey_stake},
        streamer::StakedNodes,
    },
    quinn::Connection,
    std::{future::Future, sync::RwLock},
    tokio_util::sync::CancellationToken,
};

/// A trait to provide context about a connection, such as peer type,
/// remote pubkey. This is opaque to the framework and is provided by
/// the concrete implementation of QosController.
pub(crate) trait ConnectionContext: Clone + Send + Sync {
    fn peer_type(&self) -> ConnectionPeerType;
    fn remote_pubkey(&self) -> Option<solana_pubkey::Pubkey>;
}

pub(crate) fn is_stake_current<C: ConnectionContext>(
    context: &C,
    staked_nodes: &RwLock<StakedNodes>,
) -> bool {
    let ConnectionPeerType::Staked(cached_stake) = context.peer_type() else {
        return true;
    };
    context.remote_pubkey().is_some_and(|pubkey| {
        get_pubkey_stake(&pubkey, staked_nodes)
            .is_some_and(|(stake, _total_stake)| stake == cached_stake)
    })
}

/// A trait to manage QoS for connections. This includes
/// 1) deriving the ConnectionContext for a connection
/// 2) managing connection caching and connection limits, stream limits
pub(crate) trait QosController<C: ConnectionContext> {
    /// Build the ConnectionContext for a connection
    fn build_connection_context(&self, connection: &Connection) -> C;

    /// Returns whether the stake cached in the connection context is still current.
    fn is_stake_current(&self, context: &C) -> bool;

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

    /// Optionally spawn QoS-specific background tasks onto the server runtime.
    fn spawn_background_tasks(&mut self) {}

    /// How many concurrent
    fn max_concurrent_connections(&self) -> usize;
}

/// Marker trait to indicate what is the shared state for connections
pub(crate) trait OpaqueStreamerCounter: Send + Sync + 'static {}

#[cfg(test)]
pub(crate) struct NullStreamerCounter;

#[cfg(test)]
impl OpaqueStreamerCounter for NullStreamerCounter {}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_pubkey::Pubkey,
        std::{collections::HashMap, sync::Arc},
    };

    #[derive(Clone)]
    struct TestConnectionContext {
        peer_type: ConnectionPeerType,
        remote_pubkey: Option<Pubkey>,
    }

    impl ConnectionContext for TestConnectionContext {
        fn peer_type(&self) -> ConnectionPeerType {
            self.peer_type
        }

        fn remote_pubkey(&self) -> Option<Pubkey> {
            self.remote_pubkey
        }
    }

    #[test]
    fn test_is_stake_current() {
        let pubkey = Pubkey::new_unique();
        let other_pubkey = Pubkey::new_unique();
        let staked_nodes = RwLock::new(StakedNodes::new(
            Arc::new(HashMap::from([(pubkey, 100), (other_pubkey, 900)])),
            HashMap::new(),
        ));
        let context = TestConnectionContext {
            peer_type: ConnectionPeerType::Staked(100),
            remote_pubkey: Some(pubkey),
        };
        let other_context = TestConnectionContext {
            peer_type: ConnectionPeerType::Staked(900),
            remote_pubkey: Some(other_pubkey),
        };

        assert!(is_stake_current(&context, &staked_nodes));
        assert!(is_stake_current(&other_context, &staked_nodes));
        *staked_nodes.write().unwrap() = StakedNodes::new(
            Arc::new(HashMap::from([(pubkey, 100), (other_pubkey, 1_900)])),
            HashMap::new(),
        );
        assert!(is_stake_current(&context, &staked_nodes));
        assert!(!is_stake_current(&other_context, &staked_nodes));
        *staked_nodes.write().unwrap() = StakedNodes::default();
        assert!(!is_stake_current(&context, &staked_nodes));
    }
}
