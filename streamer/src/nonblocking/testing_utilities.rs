//! Contains utility functions to create server and client for test purposes.
use {
    super::quic::{SpawnNonBlockingServerResult, ALPN_TPU_PROTOCOL_ID},
    crate::{
        nonblocking::{
            quic::spawn_server,
            swqos::{SwQos, SwQosConfig},
        },
        quic::{QuicServerError, QuicStreamerConfig, StreamerStats},
        streamer::StakedNodes,
    },
    crossbeam_channel::{unbounded, Receiver, Sender},
    quinn::{
        crypto::rustls::QuicClientConfig, ClientConfig, Connection, EndpointConfig, IdleTimeout,
        TokioRuntime, TransportConfig,
    },
    solana_keypair::Keypair,
    solana_net_utils::sockets::{
        bind_to_localhost_unique, localhost_port_range_for_tests, multi_bind_in_range_with_config,
        SocketConfiguration as SocketConfig,
    },
    solana_perf::packet::PacketBatch,
    solana_quic_definitions::{QUIC_KEEP_ALIVE, QUIC_MAX_TIMEOUT, QUIC_SEND_FAIRNESS},
    solana_tls_utils::{new_dummy_x509_certificate, tls_client_config_builder},
    std::{
        net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
        sync::{Arc, RwLock},
        time::{Duration, Instant},
    },
    tokio::{task::JoinHandle, time::sleep},
    tokio_util::sync::CancellationToken,
};

/// Spawn a streamer instance in the current tokio runtime.
pub fn spawn_stake_weighted_qos_server(
    name: &'static str,
    sockets: impl IntoIterator<Item = UdpSocket>,
    keypair: &Keypair,
    packet_sender: Sender<PacketBatch>,
    staked_nodes: Arc<RwLock<StakedNodes>>,
    quic_server_params: QuicStreamerConfig,
    qos_config: SwQosConfig,
    cancel: CancellationToken,
) -> Result<SpawnNonBlockingServerResult, QuicServerError>
where
{
    let stats = Arc::<StreamerStats>::default();

    let swqos = Arc::new(SwQos::new(
        qos_config,
        stats.clone(),
        staked_nodes,
        cancel.clone(),
    ));

    spawn_server(
        name,
        stats,
        sockets,
        keypair,
        packet_sender,
        quic_server_params,
        swqos,
        cancel,
    )
}

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

pub struct SpawnTestServerResult {
    pub join_handle: JoinHandle<()>,
    pub receiver: crossbeam_channel::Receiver<PacketBatch>,
    pub server_address: SocketAddr,
    pub stats: Arc<StreamerStats>,
    pub cancel: CancellationToken,
}

pub fn create_quic_server_sockets() -> Vec<UdpSocket> {
    let num = if cfg!(not(target_os = "windows")) {
        10
    } else {
        1
    };
    let port_range = localhost_port_range_for_tests();
    multi_bind_in_range_with_config(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        port_range,
        SocketConfig::default(),
        num,
    )
    .expect("bind operation for quic server sockets should succeed")
    .1
}

pub fn setup_quic_server(
    option_staked_nodes: Option<StakedNodes>,
    quic_server_params: QuicStreamerConfig,
    qos_config: SwQosConfig,
) -> SpawnTestServerResult {
    let sockets = create_quic_server_sockets();
    let (sender, receiver) = unbounded();
    let keypair = Keypair::new();
    let server_address = sockets[0].local_addr().unwrap();
    let staked_nodes = Arc::new(RwLock::new(option_staked_nodes.unwrap_or_default()));
    let cancel = CancellationToken::new();

    let SpawnNonBlockingServerResult {
        endpoints: _,
        stats,
        thread: handle,
        max_concurrent_connections: _,
    } = spawn_stake_weighted_qos_server(
        "quic_streamer_test",
        sockets,
        &keypair,
        sender,
        staked_nodes,
        quic_server_params,
        qos_config,
        cancel.clone(),
    )
    .unwrap();
    SpawnTestServerResult {
        join_handle: handle,
        receiver,
        server_address,
        stats,
        cancel,
    }
}

pub async fn make_client_endpoint(
    addr: &SocketAddr,
    client_keypair: Option<&Keypair>,
) -> Connection {
    let client_socket = bind_to_localhost_unique().expect("should bind - client");
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
        info!("sending: {i}");
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
