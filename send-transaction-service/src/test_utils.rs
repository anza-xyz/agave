//! This module contains functionality required to create tests parameterized
//! with the client type.

use {
    crate::{tpu_info::NullTpuInfo, transaction_client::TpuClientNextClient},
    solana_net_utils::sockets::{bind_to, localhost_port_range_for_tests},
    std::net::{IpAddr, Ipv4Addr, SocketAddr},
    tokio::runtime::Handle,
    tokio_util::sync::CancellationToken,
};

pub fn create_client_for_tests(
    runtime_handle: Handle,
    my_tpu_address: SocketAddr,
    tpu_peers: Option<Vec<SocketAddr>>,
    leader_forward_count: u64,
) -> TpuClientNextClient {
    let port_range = localhost_port_range_for_tests();
    let bind_socket = bind_to(IpAddr::V4(Ipv4Addr::LOCALHOST), port_range.0)
        .expect("Should be able to open UdpSocket for tests.");
    TpuClientNextClient::new::<NullTpuInfo>(
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
