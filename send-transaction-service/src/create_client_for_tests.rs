//! This module contains functionality required to create tests parametrized
//! with the client type.

use {
    crate::{
        tpu_info::NullTpuInfo,
        transaction_client::TransactionClient,
        transaction_client::{
            spawn_tpu_client_send_txs, Cancelable, ConnectionCacheClient, TpuClientNextClient,
        },
    },
    solana_client::connection_cache::ConnectionCache,
    solana_quic_client::quic_client::RUNTIME,
    std::{net::SocketAddr, sync::Arc},
};

pub trait CreateClient: TransactionClient {
    fn create_client(
        my_tpu_address: SocketAddr,
        tpu_peers: Option<Vec<SocketAddr>>,
        leader_forward_count: u64,
    ) -> Self;
}

impl CreateClient for ConnectionCacheClient<NullTpuInfo> {
    fn create_client(
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
        my_tpu_address: SocketAddr,
        tpu_peers: Option<Vec<SocketAddr>>,
        leader_forward_count: u64,
    ) -> Self {
        let runtime = &*RUNTIME;
        spawn_tpu_client_send_txs::<NullTpuInfo>(
            runtime,
            my_tpu_address,
            tpu_peers,
            None,
            leader_forward_count,
            None,
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
