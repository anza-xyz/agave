#[cfg(test)]
mod tests {
    use {
        crossbeam_channel::{unbounded, Receiver},
        log::*,
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
            quic::{QuicStreamerConfig, SpawnServerResult},
            streamer::StakedNodes,
        },
        solana_tls_utils::{new_dummy_x509_certificate, QuicClientCertificate},
        std::{
            net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
            sync::{Arc, RwLock},
            time::{Duration, Instant},
        },
        tokio::time::sleep,
        tokio_util::sync::CancellationToken,
    };

    fn check_packets(
        receiver: Receiver<PacketBatch>,
        num_bytes: usize,
        num_expected_packets: usize,
    ) {
        let mut all_packets = vec![];
        let now = Instant::now();
        let mut total_packets: usize = 0;
        while now.elapsed().as_secs() < 10 {
            if let Ok(packets) = receiver.recv_timeout(Duration::from_secs(1)) {
                total_packets = total_packets.saturating_add(packets.len());
                all_packets.push(packets)
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

    fn server_args() -> (UdpSocket, CancellationToken, Keypair) {
        let port_range = localhost_port_range_for_tests();
        (
            bind_to(IpAddr::V4(Ipv4Addr::LOCALHOST), port_range.0).expect("should bind"),
            CancellationToken::new(),
            Keypair::new(),
        )
    }

    #[test]
    fn test_quic_client_multiple_writes() {
        use {
            solana_connection_cache::client_connection::ClientConnection,
            solana_quic_client::quic_client::QuicClientConnection,
        };
        agave_logger::setup();
        let (sender, receiver) = unbounded();
        let staked_nodes = Arc::new(RwLock::new(StakedNodes::default()));
        let (s, cancel, keypair) = server_args();
        let SpawnServerResult {
            endpoints: _,
            thread: t,
            key_updater: _,
        } = solana_streamer::quic::spawn_stake_wighted_qos_server(
            "solQuicTest",
            "quic_streamer_test",
            vec![s.try_clone().unwrap()],
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

        assert!(client.send_data_batch_async(packets).is_ok());

        check_packets(receiver, num_bytes, num_expected_packets);
        cancel.cancel();
        t.join().unwrap();
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
            vec![s.try_clone().unwrap()],
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

    #[test]
    fn test_quic_bi_direction() {
        /// This tests bi-directional quic communication. There are the following components
        /// The request receiver -- responsible for receiving requests
        /// The request sender -- responsible sending requests to the request receiver using quic
        /// The response receiver -- responsible for receiving the responses to the requests
        /// The response sender -- responsible for sending responses to the response receiver.
        /// In this we demonstrate that the request sender and the response receiver use the
        /// same quic Endpoint, and the same UDP socket.
        use {
            solana_connection_cache::client_connection::ClientConnection,
            solana_quic_client::quic_client::QuicClientConnection,
        };
        agave_logger::setup();

        // Request Receiver
        let (sender, receiver) = unbounded();
        let staked_nodes = Arc::new(RwLock::new(StakedNodes::default()));
        let (request_recv_socket, request_recv_cancel, keypair) = server_args();
        let SpawnServerResult {
            endpoints: request_recv_endpoints,
            thread: request_recv_thread,
            key_updater: _,
        } = solana_streamer::quic::spawn_stake_wighted_qos_server(
            "solQuicTest",
            "quic_streamer_test",
            [request_recv_socket.try_clone().unwrap()],
            &keypair,
            sender,
            staked_nodes.clone(),
            QuicStreamerConfig::default_for_tests(),
            SwQosConfig::default(),
            request_recv_cancel.clone(),
        )
        .unwrap();

        drop(request_recv_endpoints);
        // Response Receiver:
        let (response_recv_socket, response_recv_cancel, keypair2) = server_args();
        let (sender2, receiver2) = unbounded();

        let addr = response_recv_socket.local_addr().unwrap().ip();
        let port = response_recv_socket.local_addr().unwrap().port();
        let server_addr = SocketAddr::new(addr, port);
        let SpawnServerResult {
            endpoints: mut response_recv_endpoints,
            thread: response_recv_thread,
            key_updater: _,
        } = solana_streamer::quic::spawn_stake_wighted_qos_server(
            "solQuicTest",
            "quic_streamer_test",
            [response_recv_socket],
            &keypair2,
            sender2,
            staked_nodes,
            QuicStreamerConfig::default_for_tests(),
            SwQosConfig::default(),
            response_recv_cancel.clone(),
        )
        .unwrap();

        // Request Sender, it uses the same endpoint as the response receiver:
        let addr = request_recv_socket.local_addr().unwrap().ip();
        let port = request_recv_socket.local_addr().unwrap().port();
        let tpu_addr = SocketAddr::new(addr, port);
        let connection_cache_stats = Arc::new(ConnectionCacheStats::default());

        let (cert, priv_key) = new_dummy_x509_certificate(&Keypair::new());
        let client_certificate = Arc::new(QuicClientCertificate {
            certificate: cert,
            key: priv_key,
        });

        let response_recv_endpoint = response_recv_endpoints
            .pop()
            .expect("at least one endpoint");
        drop(response_recv_endpoints);
        let endpoint =
            QuicLazyInitializedEndpoint::new(client_certificate, Some(response_recv_endpoint));
        let request_sender =
            QuicClientConnection::new(Arc::new(endpoint), tpu_addr, connection_cache_stats);
        // Send a full size packet with single byte writes as a request.
        let num_bytes = PACKET_DATA_SIZE;
        let num_expected_packets: usize = 3000;
        let packets = vec![vec![0u8; PACKET_DATA_SIZE]; num_expected_packets];

        assert!(request_sender.send_data_batch_async(packets).is_ok());
        check_packets(receiver, num_bytes, num_expected_packets);
        info!("Received requests!");

        // Response sender
        let (cert, priv_key) = new_dummy_x509_certificate(&Keypair::new());

        let client_certificate2 = Arc::new(QuicClientCertificate {
            certificate: cert,
            key: priv_key,
        });

        let endpoint2 = QuicLazyInitializedEndpoint::new(client_certificate2, None);
        let connection_cache_stats2 = Arc::new(ConnectionCacheStats::default());
        let response_sender =
            QuicClientConnection::new(Arc::new(endpoint2), server_addr, connection_cache_stats2);

        // Send a full size packet with single byte writes.
        let num_bytes = PACKET_DATA_SIZE;
        let num_expected_packets: usize = 3000;
        let packets = vec![vec![0u8; PACKET_DATA_SIZE]; num_expected_packets];

        assert!(response_sender.send_data_batch_async(packets).is_ok());
        check_packets(receiver2, num_bytes, num_expected_packets);
        info!("Received responses!");

        // Drop the clients explicitly to avoid hung on drops
        drop(request_sender);
        drop(response_sender);

        request_recv_cancel.cancel();
        request_recv_thread.join().unwrap();
        info!("Request receiver exited!");

        response_recv_cancel.cancel();
        response_recv_thread.join().unwrap();
        info!("Response receiver exited!");
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
            vec![s.try_clone().unwrap()],
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
