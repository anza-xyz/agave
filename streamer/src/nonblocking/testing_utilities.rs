//! Contains utility functions to create server and client for test purposes.
use {
    super::quic::{
        spawn_server_multi, SpawnNonBlockingServerResult, ALPN_TPU_PROTOCOL_ID,
        DEFAULT_WAIT_FOR_CHUNK_TIMEOUT,
    },
    crate::{
        quic::{
            QuicServerParams, StreamerStats, DEFAULT_MAX_CONNECTIONS_PER_IPADDR_PER_MINUTE,
            DEFAULT_MAX_STAKED_CONNECTIONS, DEFAULT_MAX_STREAMS_PER_MS,
            DEFAULT_MAX_UNSTAKED_CONNECTIONS, DEFAULT_TPU_COALESCE,
        },
        streamer::StakedNodes,
    },
    crossbeam_channel::{unbounded, Receiver},
    quinn::{
        crypto::rustls::QuicClientConfig, ClientConfig, Connection, EndpointConfig, IdleTimeout,
        TokioRuntime, TransportConfig,
    },
    solana_keypair::Keypair,
    solana_net_utils::bind_to_localhost,
    solana_perf::packet::PacketBatch,
    solana_quic_definitions::{QUIC_KEEP_ALIVE, QUIC_MAX_TIMEOUT, QUIC_SEND_FAIRNESS},
    solana_tls_utils::{new_dummy_x509_certificate, tls_client_config_builder},
    std::{
        net::{SocketAddr, UdpSocket},
        sync::{atomic::AtomicBool, Arc, RwLock},
        time::{Duration, Instant},
    },
    tokio::{task::JoinHandle, time::sleep},
};

pub fn get_client_config(keypair: &Keypair) -> ClientConfig {
    let (cert, key) = new_dummy_x509_certificate(keypair);

    let mut crypto = tls_client_config_builder()
        .with_client_auth_cert(vec![cert], key)
        .expect("Failed to use client certificate");

    crypto.enable_early_data = true;
    crypto.alpn_protocols = vec![ALPN_TPU_PROTOCOL_ID.to_vec()];

    let mut config = ClientConfig::new(Arc::new(QuicClientConfig::try_from(crypto).unwrap()));

    let mut transport_config = TransportConfig::default();
    let timeout = IdleTimeout::try_from(QUIC_MAX_TIMEOUT).unwrap();
    transport_config.max_idle_timeout(Some(timeout));
    transport_config.keep_alive_interval(Some(QUIC_KEEP_ALIVE));
    transport_config.send_fairness(QUIC_SEND_FAIRNESS);
    config.transport_config(Arc::new(transport_config));

    config
}

#[derive(Debug, Clone)]
pub struct TestServerConfig {
    pub max_connections_per_peer: usize,
    pub max_staked_connections: usize,
    pub max_unstaked_connections: usize,
    pub max_streams_per_ms: u64,
    pub max_connections_per_ipaddr_per_min: u64,
    pub coalesce_channel_size: usize,
}

impl Default for TestServerConfig {
    fn default() -> Self {
        Self {
            max_connections_per_peer: 1,
            max_staked_connections: DEFAULT_MAX_STAKED_CONNECTIONS,
            max_unstaked_connections: DEFAULT_MAX_UNSTAKED_CONNECTIONS,
            max_streams_per_ms: DEFAULT_MAX_STREAMS_PER_MS,
            max_connections_per_ipaddr_per_min: DEFAULT_MAX_CONNECTIONS_PER_IPADDR_PER_MINUTE,
            coalesce_channel_size: 100_000, // use a smaller value for test as create a huge bounded channel can take time
        }
    }
}

pub struct SpawnTestServerResult {
    pub join_handle: JoinHandle<()>,
    pub exit: Arc<AtomicBool>,
    pub receiver: crossbeam_channel::Receiver<PacketBatch>,
    pub server_address: SocketAddr,
    pub stats: Arc<StreamerStats>,
}

pub fn create_quic_server_sockets() -> Vec<UdpSocket> {
    #[cfg(not(target_os = "windows"))]
    {
        use {
            solana_net_utils::bind_to,
            std::net::{IpAddr, Ipv4Addr},
        };
        (0..10)
            .map(|_| {
                bind_to(
                    IpAddr::V4(Ipv4Addr::LOCALHOST),
                    /*port*/ 0,
                    /*reuseport:*/ true,
                )
                .unwrap()
            })
            .collect::<Vec<_>>()
    }
    #[cfg(target_os = "windows")]
    {
        vec![bind_to_localhost().unwrap()]
    }
}

pub fn setup_quic_server(
    option_staked_nodes: Option<StakedNodes>,
    config: TestServerConfig,
) -> SpawnTestServerResult {
    let sockets = create_quic_server_sockets();
    setup_quic_server_with_sockets(sockets, option_staked_nodes, config)
}

pub fn setup_quic_server_with_sockets(
    sockets: Vec<UdpSocket>,
    option_staked_nodes: Option<StakedNodes>,
    TestServerConfig {
        max_connections_per_peer,
        max_staked_connections,
        max_unstaked_connections,
        max_streams_per_ms,
        max_connections_per_ipaddr_per_min,
        coalesce_channel_size,
    }: TestServerConfig,
) -> SpawnTestServerResult {
    let exit = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = unbounded();
    let keypair = Keypair::new();
    let server_address = sockets[0].local_addr().unwrap();
    let staked_nodes = Arc::new(RwLock::new(option_staked_nodes.unwrap_or_default()));
    let quic_server_params = QuicServerParams {
        max_connections_per_peer,
        max_staked_connections,
        max_unstaked_connections,
        max_streams_per_ms,
        max_connections_per_ipaddr_per_min,
        wait_for_chunk_timeout: DEFAULT_WAIT_FOR_CHUNK_TIMEOUT,
        coalesce: DEFAULT_TPU_COALESCE,
        coalesce_channel_size,
    };
    let SpawnNonBlockingServerResult {
        endpoints: _,
        stats,
        thread: handle,
        max_concurrent_connections: _,
    } = spawn_server_multi(
        "quic_streamer_test",
        sockets,
        &keypair,
        sender,
        exit.clone(),
        staked_nodes,
        quic_server_params,
    )
    .unwrap();
    SpawnTestServerResult {
        join_handle: handle,
        exit,
        receiver,
        server_address,
        stats,
    }
}

pub async fn make_client_endpoint(
    addr: &SocketAddr,
    client_keypair: Option<&Keypair>,
) -> Connection {
    let client_socket = bind_to_localhost().unwrap();
    let mut endpoint = quinn::Endpoint::new(
        EndpointConfig::default(),
        None,
        client_socket,
        Arc::new(TokioRuntime),
    )
    .unwrap();
    let default_keypair = Keypair::new();
    endpoint.set_default_client_config(get_client_config(
        client_keypair.unwrap_or(&default_keypair),
    ));
    endpoint
        .connect(*addr, "localhost")
        .expect("Endpoint configuration should be correct")
        .await
        .expect("Test server should be already listening on 'localhost'")
}

pub async fn check_multiple_streams(
    receiver: Receiver<PacketBatch>,
    server_address: SocketAddr,
    client_keypair: Option<&Keypair>,
) {
    let conn1 = Arc::new(make_client_endpoint(&server_address, client_keypair).await);
    let conn2 = Arc::new(make_client_endpoint(&server_address, client_keypair).await);
    let mut num_expected_packets = 0;
    for i in 0..10 {
        info!("sending: {}", i);
        let c1 = conn1.clone();
        let c2 = conn2.clone();
        let mut s1 = c1.open_uni().await.unwrap();
        let mut s2 = c2.open_uni().await.unwrap();
        s1.write_all(&[0u8]).await.unwrap();
        s1.finish().unwrap();
        s2.write_all(&[0u8]).await.unwrap();
        s2.finish().unwrap();
        num_expected_packets += 2;
        sleep(Duration::from_millis(200)).await;
    }
    let mut all_packets = vec![];
    let now = Instant::now();
    let mut total_packets = 0;
    while now.elapsed().as_secs() < 10 {
        if let Ok(packets) = receiver.try_recv() {
            total_packets += packets.len();
            all_packets.push(packets)
        } else {
            sleep(Duration::from_secs(1)).await;
        }
        if total_packets == num_expected_packets {
            break;
        }
    }
    for batch in all_packets {
        for p in batch.iter() {
            assert_eq!(p.meta().size, 1);
        }
    }
    assert_eq!(total_packets, num_expected_packets);
}
