use {
    solana_connection_cache::connection_cache::{ConnectionCache, NewConnectionConfig},
    solana_quic_client::{QuicConfig, QuicConnectionManager},
    std::net::{IpAddr, Ipv4Addr, SocketAddr},
};

#[tokio::test(flavor = "current_thread")]
async fn quicpool_drop_does_not_block_in_async_context() {
    let config = QuicConfig::new().expect("create quic config");
    let manager = QuicConnectionManager::new_with_connection_config(config);
    let cache = ConnectionCache::new("quicpool_drop_async", manager, 1)
        .expect("create quic connection cache");
    const MAX_CONNECTIONS: usize = 1024;

    for i in 0..(MAX_CONNECTIONS + 1) {
        let b = ((i / 250) + 1) as u8;
        let c = ((i % 250) + 1) as u8;
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, b, c, 1)), 12345);
        let conn = cache.get_nonblocking_connection(&addr);
        drop(conn);
    }
    // Eviction should drop a QuicPool while still inside the tokio runtime.
}
