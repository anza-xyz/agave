//! This module contains functionality required to create tests parameterized
//! with the client type.

use {
    crate::{
        tpu_info::NullTpuInfo,
        transaction_client::{TpuClient, TpuSender, create_client, create_leader_updater},
    },
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
) -> (TpuSender, TpuClient) {
    let port_range = localhost_port_range_for_tests();
    let bind_socket = bind_to(IpAddr::V4(Ipv4Addr::LOCALHOST), port_range.0)
        .expect("Should be able to open UdpSocket for tests.");
    let leader_updater = create_leader_updater::<NullTpuInfo>(None, my_tpu_address, tpu_peers);
    create_client(
        runtime_handle,
        leader_updater,
        leader_forward_count,
        None,
        bind_socket,
        CancellationToken::new(),
    )
    .expect("Should be able to create TPU client for tests.")
}
