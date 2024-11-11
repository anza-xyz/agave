//! This module contains functionality required to create tests parametrized
//! with the client type.

use {
    crate::{
        tpu_info::NullTpuInfo,
        transaction_client::{
            spawn_tpu_client_send_txs, Cancelable, ConnectionCacheClient, TpuClientNextClient,
            TransactionClient,
        },
    },
    solana_client::connection_cache::ConnectionCache,
    solana_tpu_client_next::QuicClientCertificate,
    std::{net::SocketAddr, sync::Arc},
    tokio::runtime::Handle,
};

// `maybe_runtime` argument is introduced to be able to use runtime from test
// for the TpuClientNext, while ConnectionCache uses runtime created internally
// in the quic-client module and it is impossible to pass test runtime there.
pub trait CreateClient: TransactionClient {
    fn create_client(
        maybe_runtime: Option<Handle>,
        my_tpu_address: SocketAddr,
        tpu_peers: Option<Vec<SocketAddr>>,
        leader_forward_count: u64,
    ) -> Self;
}

impl CreateClient for ConnectionCacheClient<NullTpuInfo> {
    fn create_client(
        _maybe_runtime: Option<Handle>,
        my_tpu_address: SocketAddr,
        tpu_peers: Option<Vec<SocketAddr>>,
        leader_forward_count: u64,
    ) -> Self {
        let connection_cache = Arc::new(ConnectionCache::new("connection_cache_test"));
        ConnectionCacheClient::new(
            connection_cache,
            my_tpu_address,
            tpu_peers,
            None,
            leader_forward_count,
        )
    }
}

impl CreateClient for TpuClientNextClient {
    fn create_client(
        maybe_runtime: Option<Handle>,
        my_tpu_address: SocketAddr,
        tpu_peers: Option<Vec<SocketAddr>>,
        leader_forward_count: u64,
    ) -> Self {
        let runtime =
            maybe_runtime.expect("Runtime should be provided for the TpuClientNextClient.");
        spawn_tpu_client_send_txs::<NullTpuInfo>(
            runtime,
            my_tpu_address,
            tpu_peers,
            None,
            leader_forward_count,
            QuicClientCertificate::with_option(None),
        )
    }
}

// Define type alias to simplify definition of test functions.
pub trait ClientWithCreator:
    CreateClient + TransactionClient + Cancelable + Send + Clone + 'static
{
}
impl<T> ClientWithCreator for T where
    T: CreateClient + TransactionClient + Cancelable + Send + Clone + 'static
{
}
