//! This module contains functionality required to create tests parameterized
//! with the client type.

use {
    crate::{
        tpu_info::NullTpuInfo,
        transaction_client::{ConnectionCacheClient, TpuInfoWithSendStatic, TransactionClient},
    },
    solana_client::connection_cache::ConnectionCache,
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
        maybe_runtime: Option<Handle>,
        my_tpu_address: SocketAddr,
        tpu_peers: Option<Vec<SocketAddr>>,
        leader_forward_count: u64,
    ) -> Self {
        assert!(maybe_runtime.is_none());
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

pub trait Cancelable {
    fn cancel(&self);
}

impl<T> Cancelable for ConnectionCacheClient<T>
where
    T: TpuInfoWithSendStatic,
{
    fn cancel(&self) {}
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
