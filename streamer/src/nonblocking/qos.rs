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

pub(crate) fn has_sufficient_stake<C: ConnectionContext>(
    context: &C,
    staked_nodes: &RwLock<StakedNodes>,
) -> bool {
    let ConnectionPeerType::Staked(cached_stake) = context.peer_type() else {
        return true;
    };
    context.remote_pubkey().is_some_and(|pubkey| {
        get_pubkey_stake(&pubkey, staked_nodes).is_some_and(|(stake, _total_stake)| {
            // Keep the connection as long as the peer retains at least half of the stake
            // cached at handshake. This catches de-staking (no stake left) and large
            // reductions, while tolerating organic drift (epoch rewards, routine delegator
            // churn) so healthy staked connections are never recycled; a stale cache can
            // thus over-grant at most 2x. Increases never evict: a peer can reconnect to
            // claim its higher weight.
            stake.saturating_mul(2) >= cached_stake
        })
    })
}

/// A trait to manage QoS for connections. This includes
/// 1) deriving the ConnectionContext for a connection
/// 2) managing connection caching and connection limits, stream limits
pub(crate) trait QosController<C: ConnectionContext> {
    /// Build the ConnectionContext for a connection
    fn build_connection_context(&self, connection: &Connection) -> C;

    /// Returns whether the peer still retains enough stake relative to the value cached in
    /// the connection context, i.e. whether the connection may stay alive.
    fn has_sufficient_stake(&self, context: &C) -> bool;

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

    fn set_stakes(staked_nodes: &RwLock<StakedNodes>, stakes: HashMap<Pubkey, u64>) {
        *staked_nodes.write().unwrap() = StakedNodes::new(Arc::new(stakes), HashMap::new());
    }

    #[test]
    fn test_has_sufficient_stake() {
        let pubkey = Pubkey::new_unique();
        let staked_nodes = RwLock::new(StakedNodes::new(
            Arc::new(HashMap::from([(pubkey, 100)])),
            HashMap::new(),
        ));
        let context = TestConnectionContext {
            peer_type: ConnectionPeerType::Staked(100),
            remote_pubkey: Some(pubkey),
        };
        let unstaked_context = TestConnectionContext {
            peer_type: ConnectionPeerType::Unstaked,
            remote_pubkey: None,
        };

        assert!(
            has_sufficient_stake(&context, &staked_nodes),
            "unchanged stake should keep the connection"
        );
        set_stakes(&staked_nodes, HashMap::from([(pubkey, 1_000)]));
        assert!(
            has_sufficient_stake(&context, &staked_nodes),
            "increased stake should keep the connection"
        );
        set_stakes(&staked_nodes, HashMap::from([(pubkey, 50)]));
        assert!(
            has_sufficient_stake(&context, &staked_nodes),
            "stake at half of the cached value should keep the connection"
        );
        set_stakes(&staked_nodes, HashMap::from([(pubkey, 49)]));
        assert!(
            !has_sufficient_stake(&context, &staked_nodes),
            "stake below half of the cached value should evict the connection"
        );
        set_stakes(&staked_nodes, HashMap::new());
        assert!(
            !has_sufficient_stake(&context, &staked_nodes),
            "full de-staking should evict the connection"
        );
        assert!(
            has_sufficient_stake(&unstaked_context, &staked_nodes),
            "unstaked connections should not be evicted by stake revalidation"
        );
    }
}
