#[cfg(test)]
mod tests {
    use {
        crossbeam_channel::{Receiver, unbounded},
        solana_connection_cache::{
            client_connection::ClientStats, connection_cache_stats::ConnectionCacheStats,
        },
        solana_keypair::Keypair,
        solana_net_utils::sockets::{bind_to, localhost_port_range_for_tests},
        solana_packet::PACKET_DATA_SIZE,
        solana_perf::packet::PacketBatch,
        solana_quic_client::nonblocking::quic_client::{QuicClient, QuicLazyInitializedEndpoint},
        solana_streamer::{
            nonblocking::{quic::SpawnNonBlockingServerResult, swqos::SwQosConfig},
            quic::QuicStreamerConfig,
            streamer::StakedNodes,
        },
        std::{
            net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
            sync::{Arc, RwLock},
            time::{Duration, Instant},
        },
        tokio::time::sleep,
        tokio_util::sync::CancellationToken,
    };

    fn server_args() -> (UdpSocket, CancellationToken, Keypair) {
        let port_range = localhost_port_range_for_tests();
        (
            bind_to(IpAddr::V4(Ipv4Addr::LOCALHOST), port_range.0).expect("should bind"),
            CancellationToken::new(),
            Keypair::new(),
        )
    }

    #[tokio::test]
    async fn test_quic_connection_cache_update_key_from_tokio_runtime() {
        use {
            solana_connection_cache::connection_cache::{ConnectionCache, NewConnectionConfig},
            solana_quic_client::{QuicConfig, QuicConnectionManager},
        };

        let keypair = Keypair::new();
        let mut config = QuicConfig::new().unwrap();
        config.update_client_certificate(&keypair, IpAddr::V4(Ipv4Addr::LOCALHOST));
        let connection_manager = QuicConnectionManager::new_with_connection_config(config);
        let connection_cache = ConnectionCache::new(
            "quic_connection_cache_update_key_test",
            connection_manager,
            1,
        )
        .unwrap();

        let server_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 12345);
        let _connection = connection_cache.get_nonblocking_connection(&server_addr);

        connection_cache.update_key(&Keypair::new()).unwrap();
    }

    // A version of check_packets that avoids blocking in an
    // async environment. todo: we really need a way of guaranteeing
    // we don't block in async code/tests, as it can lead to subtle bugs
    // that don't immediately manifest, but only show up when a separate
    // change (often itself valid) is made
    async fn nonblocking_check_packets(
        receiver: Receiver<PacketBatch>,
        num_bytes: usize,
        num_expected_packets: usize,
    ) {
        let mut all_packets = vec![];
        let now = Instant::now();
        let mut total_packets: usize = 0;
        while now.elapsed().as_secs() < 10 {
            if let Ok(packets) = receiver.try_recv() {
                total_packets = total_packets.saturating_add(packets.len());
                all_packets.push(packets)
            } else {
                sleep(Duration::from_secs(1)).await;
            }
            if total_packets >= num_expected_packets {
                break;
            }
        }
        for batch in all_packets {
            for p in &batch {
                assert_eq!(p.meta().size, num_bytes);
            }
        }
        assert!(total_packets > 0);
    }

    #[tokio::test]
    async fn test_nonblocking_quic_client_multiple_writes() {
        use {
            solana_connection_cache::nonblocking::client_connection::ClientConnection,
            solana_quic_client::nonblocking::quic_client::QuicClientConnection,
        };
        agave_logger::setup();
        let (sender, receiver) = unbounded();
        let staked_nodes = Arc::new(RwLock::new(StakedNodes::default()));
        let (s, cancel, keypair) = server_args();
        let SpawnNonBlockingServerResult {
            endpoints: _,
            stats: _,
            thread: t,
            max_concurrent_connections: _,
        } = solana_streamer::nonblocking::testing_utilities::spawn_stake_weighted_qos_server(
            "quic_streamer_test",
            vec![s.try_clone().unwrap().into()],
            &keypair,
            sender,
            staked_nodes,
            QuicStreamerConfig::default_for_tests(),
            SwQosConfig::default(),
            cancel.clone(),
        )
        .unwrap();

        let addr = s.local_addr().unwrap().ip();
        let port = s.local_addr().unwrap().port();
        let tpu_addr = SocketAddr::new(addr, port);
        let connection_cache_stats = Arc::new(ConnectionCacheStats::default());
        let client = QuicClientConnection::new(
            Arc::new(QuicLazyInitializedEndpoint::default()),
            tpu_addr,
            connection_cache_stats,
        );

        // Send a full size packet with single byte writes.
        let num_bytes = PACKET_DATA_SIZE;
        let num_expected_packets: usize = 3000;
        let packets = vec![vec![0u8; PACKET_DATA_SIZE]; num_expected_packets];
        for packet in packets {
            let _ = client.send_data(&packet).await;
        }

        nonblocking_check_packets(receiver, num_bytes, num_expected_packets).await;
        cancel.cancel();
        t.await.unwrap();
    }

    #[tokio::test]
    async fn test_connection_close() {
        agave_logger::setup();
        let (sender, receiver) = unbounded();
        let staked_nodes = Arc::new(RwLock::new(StakedNodes::default()));
        let (s, cancel, keypair) = server_args();
        let solana_streamer::nonblocking::quic::SpawnNonBlockingServerResult {
            endpoints: _,
            stats: _,
            thread: t,
            max_concurrent_connections: _,
        } = solana_streamer::nonblocking::testing_utilities::spawn_stake_weighted_qos_server(
            "quic_streamer_test",
            vec![s.try_clone().unwrap().into()],
            &keypair,
            sender,
            staked_nodes,
            QuicStreamerConfig::default_for_tests(),
            SwQosConfig::default(),
            cancel.clone(),
        )
        .unwrap();

        let addr = s.local_addr().unwrap().ip();
        let port = s.local_addr().unwrap().port();
        let tpu_addr = SocketAddr::new(addr, port);
        let connection_cache_stats = Arc::new(ConnectionCacheStats::default());
        let client = QuicClient::new(Arc::new(QuicLazyInitializedEndpoint::default()), tpu_addr);

        // Send a full size packet with single byte writes.
        let num_bytes = PACKET_DATA_SIZE;
        let num_expected_packets: usize = 3;
        let packets = vec![vec![0u8; PACKET_DATA_SIZE]; num_expected_packets];
        let client_stats = ClientStats::default();
        for packet in packets {
            let _ = client
                .send_buffer(&packet, &client_stats, connection_cache_stats.clone())
                .await;
        }

        nonblocking_check_packets(receiver, num_bytes, num_expected_packets).await;
        cancel.cancel();

        t.await.unwrap();
        // We close the connection after the server is down, this should not block
        client.close().await;
    }
}
