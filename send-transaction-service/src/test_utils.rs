//! This module contains functionality required to create tests parameterized
//! with the client type.

use {
    crate::{
        tpu_info::NullTpuInfo,
        transaction_client::{
            ConnectionCacheClient, TpuClientNextClient, TpuInfoWithSendStatic, TransactionClient,
        },
    },
    solana_client::connection_cache::ConnectionCache,
    solana_net_utils::sockets::{bind_to, localhost_port_range_for_tests},
    std::{
        net::{IpAddr, Ipv4Addr, SocketAddr},
        sync::Arc,
    },
    tokio::runtime::Handle,
    tokio_util::sync::CancellationToken,
};

// `maybe_runtime` argument is introduced to be able to use runtime from test
// for the TpuClientNext, while ConnectionCache uses runtime created internally
// in the quic-client module and it is impossible to pass test runtime there.
pub trait CreateClient: TransactionClient + Clone {
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
        let runtime_handle =
            maybe_runtime.expect("Runtime should be provided for the TpuClientNextClient.");
        let port_range = localhost_port_range_for_tests();
        let bind_socket = bind_to(IpAddr::V4(Ipv4Addr::LOCALHOST), port_range.0)
            .expect("Should be able to open UdpSocket for tests.");
        Self::new::<NullTpuInfo>(
            runtime_handle,
            my_tpu_address,
            tpu_peers,
            None,
            leader_forward_count,
            None,
            bind_socket,
            CancellationToken::new(),
        )
    }
}

pub trait Stoppable {
    fn stop(&self);
}

impl<T> Stoppable for ConnectionCacheClient<T>
where
    T: TpuInfoWithSendStatic,
{
    fn stop(&self) {}
}

impl Stoppable for TpuClientNextClient {
    fn stop(&self) {
        self.cancel();
    }
}

// Define type alias to simplify definition of test functions.
pub trait ClientWithCreator:
    CreateClient + TransactionClient + Stoppable + Send + Clone + 'static
{
}
impl<T> ClientWithCreator for T where
    T: CreateClient + TransactionClient + Stoppable + Send + Clone + 'static
{
}
