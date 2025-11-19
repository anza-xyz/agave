use {
    crate::{
        nonblocking::{
            qos::{ConnectionContext, QosController},
            quic::{ALPN_TPU_PROTOCOL_ID, DEFAULT_WAIT_FOR_CHUNK_TIMEOUT},
            simple_qos::{SimpleQos, SimpleQosConfig},
            swqos::{SwQos, SwQosConfig},
        },
        streamer::StakedNodes,
    },
    crossbeam_channel::Sender,
    pem::Pem,
    quinn::{
        crypto::rustls::{NoInitialCipherSuite, QuicServerConfig},
        Endpoint, IdleTimeout, ServerConfig,
    },
    rustls::KeyLogFile,
    solana_keypair::Keypair,
    solana_packet::PACKET_DATA_SIZE,
    solana_perf::packet::PacketBatch,
    solana_quic_definitions::{
        NotifyKeyUpdate, QUIC_MAX_TIMEOUT, QUIC_MAX_UNSTAKED_CONCURRENT_STREAMS,
    },
    solana_tls_utils::{new_dummy_x509_certificate, tls_server_config_builder},
    std::{
        net::UdpSocket,
        num::NonZeroUsize,
        sync::{
            atomic::{AtomicU64, AtomicUsize, Ordering},
            Arc, Mutex, RwLock,
        },
        thread::{self},
        time::Duration,
    },
    tokio::runtime::Runtime,
    tokio_util::sync::CancellationToken,
};

// allow multiple connections for NAT and any open/close overlap
pub const DEFAULT_MAX_QUIC_CONNECTIONS_PER_UNSTAKED_PEER: usize = 8;

// allow multiple connections per ID for geo-distributed forwarders
pub const DEFAULT_MAX_QUIC_CONNECTIONS_PER_STAKED_PEER: usize = 16;

pub const DEFAULT_MAX_STAKED_CONNECTIONS: usize = 2000;

pub const DEFAULT_MAX_UNSTAKED_CONNECTIONS: usize = 4000;

/// Limit to 500K PPS
pub const DEFAULT_MAX_STREAMS_PER_MS: u64 = 500;

/// The new connections per minute from a particular IP address.
/// Heuristically set to the default maximum concurrent connections
/// per IP address. Might be adjusted later.
pub const DEFAULT_MAX_CONNECTIONS_PER_IPADDR_PER_MINUTE: u64 = 8;

// This will be adjusted and parameterized in follow-on PRs.
pub const DEFAULT_QUIC_ENDPOINTS: usize = 1;

pub fn default_num_tpu_transaction_forward_receive_threads() -> usize {
    num_cpus::get().min(16)
}

pub fn default_num_tpu_transaction_receive_threads() -> usize {
    num_cpus::get().min(8)
}

pub fn default_num_tpu_vote_transaction_receive_threads() -> usize {
    num_cpus::get().min(8)
}

pub struct SpawnServerResult {
    pub endpoints: Vec<Endpoint>,
    pub thread: thread::JoinHandle<()>,
    pub key_updater: Arc<EndpointKeyUpdater>,
}

/// Returns default server configuration along with its PEM certificate chain.
#[allow(clippy::field_reassign_with_default)] // https://github.com/rust-lang/rust-clippy/issues/6527
pub(crate) fn configure_server(
    identity_keypair: &Keypair,
) -> Result<(ServerConfig, String), QuicServerError> {
    let (cert, priv_key) = new_dummy_x509_certificate(identity_keypair);
    let cert_chain_pem_parts = vec![Pem {
        tag: "CERTIFICATE".to_string(),
        contents: cert.as_ref().to_vec(),
    }];
    let cert_chain_pem = pem::encode_many(&cert_chain_pem_parts);

    let mut server_tls_config =
        tls_server_config_builder().with_single_cert(vec![cert], priv_key)?;
    server_tls_config.alpn_protocols = vec![ALPN_TPU_PROTOCOL_ID.to_vec()];
    server_tls_config.key_log = Arc::new(KeyLogFile::new());
    let quic_server_config = QuicServerConfig::try_from(server_tls_config)?;

    let mut server_config = ServerConfig::with_crypto(Arc::new(quic_server_config));

    // disable path migration as we do not expect TPU clients to be on a mobile device
    server_config.migration(false);

    let config = Arc::get_mut(&mut server_config.transport).unwrap();

    // QUIC_MAX_CONCURRENT_STREAMS doubled, which was found to improve reliability
    const MAX_CONCURRENT_UNI_STREAMS: u32 =
        (QUIC_MAX_UNSTAKED_CONCURRENT_STREAMS.saturating_mul(2)) as u32;
    config.max_concurrent_uni_streams(MAX_CONCURRENT_UNI_STREAMS.into());
    config.stream_receive_window((PACKET_DATA_SIZE as u32).into());
    config.receive_window((PACKET_DATA_SIZE as u32).into());
    let timeout = IdleTimeout::try_from(QUIC_MAX_TIMEOUT).unwrap();
    config.max_idle_timeout(Some(timeout));

    // disable bidi & datagrams
    const MAX_CONCURRENT_BIDI_STREAMS: u32 = 0;
    config.max_concurrent_bidi_streams(MAX_CONCURRENT_BIDI_STREAMS.into());
    config.datagram_receive_buffer_size(None);

    // Disable GSO. The server only accepts inbound unidirectional streams initiated by clients,
    // which means that reply data never exceeds one MTU. By disabling GSO, we make
    // quinn_proto::Connection::poll_transmit allocate only 1 MTU vs 10 * MTU for _each_ transmit.
    // See https://github.com/anza-xyz/agave/pull/1647.
    config.enable_segmentation_offload(false);

    Ok((server_config, cert_chain_pem))
}

fn rt(name: String, num_threads: NonZeroUsize) -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .thread_name(name)
        .worker_threads(num_threads.get())
        .enable_all()
        .build()
        .unwrap()
}

#[derive(thiserror::Error, Debug)]
pub enum QuicServerError {
    #[error("Endpoint creation failed: {0}")]
    EndpointFailed(std::io::Error),
    #[error("TLS error: {0}")]
    TlsError(#[from] rustls::Error),
    #[error("No initial cipher suite")]
    NoInitialCipherSuite(#[from] NoInitialCipherSuite),
}

pub struct EndpointKeyUpdater {
    endpoints: Vec<Endpoint>,
}

impl NotifyKeyUpdate for EndpointKeyUpdater {
    fn update_key(&self, key: &Keypair) -> Result<(), Box<dyn std::error::Error>> {
        let (config, _) = configure_server(key)?;
        for endpoint in &self.endpoints {
            endpoint.set_server_config(Some(config.clone()));
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct StreamerStats {
    pub(crate) total_connections: AtomicUsize,
    pub(crate) total_new_connections: AtomicUsize,
    pub(crate) active_streams: AtomicUsize,
    pub(crate) total_new_streams: AtomicUsize,
    pub(crate) invalid_stream_size: AtomicUsize,
    pub(crate) total_staked_chunks_received: AtomicUsize,
    pub(crate) total_unstaked_chunks_received: AtomicUsize,
    pub(crate) total_handle_chunk_to_packet_send_err: AtomicUsize,
    pub(crate) total_handle_chunk_to_packet_send_full_err: AtomicUsize,
    pub(crate) total_handle_chunk_to_packet_send_disconnected_err: AtomicUsize,
    pub(crate) total_packet_batches_none: AtomicUsize,
    pub(crate) total_packets_sent_to_consumer: AtomicUsize,
    pub(crate) total_bytes_sent_to_consumer: AtomicUsize,
    pub(crate) total_chunks_processed_by_batcher: AtomicUsize,
    pub(crate) total_stream_read_errors: AtomicUsize,
    pub(crate) total_stream_read_timeouts: AtomicUsize,
    pub(crate) num_evictions_staked: AtomicUsize,
    pub(crate) num_evictions_unstaked: AtomicUsize,
    pub(crate) connection_added_from_staked_peer: AtomicUsize,
    pub(crate) connection_added_from_unstaked_peer: AtomicUsize,
    pub(crate) connection_add_failed: AtomicUsize,
    pub(crate) connection_add_failed_invalid_stream_count: AtomicUsize,
    pub(crate) connection_add_failed_staked_node: AtomicUsize,
    pub(crate) connection_add_failed_unstaked_node: AtomicUsize,
    pub(crate) connection_add_failed_on_pruning: AtomicUsize,
    pub(crate) connection_setup_timeout: AtomicUsize,
    pub(crate) connection_setup_error: AtomicUsize,
    pub(crate) connection_setup_error_closed: AtomicUsize,
    pub(crate) connection_setup_error_timed_out: AtomicUsize,
    pub(crate) connection_setup_error_transport: AtomicUsize,
    pub(crate) connection_setup_error_app_closed: AtomicUsize,
    pub(crate) connection_setup_error_reset: AtomicUsize,
    pub(crate) connection_setup_error_locally_closed: AtomicUsize,
    pub(crate) connection_removed: AtomicUsize,
    pub(crate) connection_remove_failed: AtomicUsize,
    // Number of connections to the endpoint exceeding the allowed limit
    // regardless of the source IP address.
    pub(crate) connection_rate_limited_across_all: AtomicUsize,
    // Per IP rate-limiting is triggered each time when there are too many connections
    // opened from a particular IP address.
    pub(crate) connection_rate_limited_per_ipaddr: AtomicUsize,
    pub(crate) throttled_streams: AtomicUsize,
    pub(crate) stream_load_ema: AtomicUsize,
    pub(crate) stream_load_ema_overflow: AtomicUsize,
    pub(crate) stream_load_capacity_overflow: AtomicUsize,
    pub(crate) process_sampled_packets_us_hist: Mutex<histogram::Histogram>,
    pub(crate) perf_track_overhead_us: AtomicU64,
    pub(crate) total_staked_packets_sent_for_batching: AtomicUsize,
    pub(crate) total_unstaked_packets_sent_for_batching: AtomicUsize,
    pub(crate) throttled_staked_streams: AtomicUsize,
    pub(crate) throttled_unstaked_streams: AtomicUsize,
    // All connections in various states such as Incoming, Connecting, Connection
    pub(crate) open_connections: AtomicUsize,
    pub(crate) open_staked_connections: AtomicUsize,
    pub(crate) open_unstaked_connections: AtomicUsize,
    pub(crate) peak_open_staked_connections: AtomicUsize,
    pub(crate) peak_open_unstaked_connections: AtomicUsize,
    pub(crate) refused_connections_too_many_open_connections: AtomicUsize,
    pub(crate) outstanding_incoming_connection_attempts: AtomicUsize,
    pub(crate) total_incoming_connection_attempts: AtomicUsize,
    pub(crate) quic_endpoints_count: AtomicUsize,
}

impl StreamerStats {
    pub fn report(&self, name: &'static str) {
        let process_sampled_packets_us_hist = {
            let mut metrics = self.process_sampled_packets_us_hist.lock().unwrap();
            let process_sampled_packets_us_hist = metrics.clone();
            metrics.clear();
            process_sampled_packets_us_hist
        };

        datapoint_info!(
            name,
            (
                "active_connections",
                self.total_connections.load(Ordering::Relaxed),
                i64
            ),
            (
                "active_streams",
                self.active_streams.load(Ordering::Relaxed),
                i64
            ),
            (
                "new_connections",
                self.total_new_connections.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "new_streams",
                self.total_new_streams.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "evictions_staked",
                self.num_evictions_staked.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "evictions_unstaked",
                self.num_evictions_unstaked.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_added_from_staked_peer",
                self.connection_added_from_staked_peer
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_added_from_unstaked_peer",
                self.connection_added_from_unstaked_peer
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_add_failed",
                self.connection_add_failed.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_add_failed_invalid_stream_count",
                self.connection_add_failed_invalid_stream_count
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_add_failed_staked_node",
                self.connection_add_failed_staked_node
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_add_failed_unstaked_node",
                self.connection_add_failed_unstaked_node
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_add_failed_on_pruning",
                self.connection_add_failed_on_pruning
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_removed",
                self.connection_removed.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_remove_failed",
                self.connection_remove_failed.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_setup_timeout",
                self.connection_setup_timeout.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_setup_error",
                self.connection_setup_error.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_setup_error_timed_out",
                self.connection_setup_error_timed_out
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_setup_error_closed",
                self.connection_setup_error_closed
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_setup_error_transport",
                self.connection_setup_error_transport
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_setup_error_app_closed",
                self.connection_setup_error_app_closed
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_setup_error_reset",
                self.connection_setup_error_reset.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_setup_error_locally_closed",
                self.connection_setup_error_locally_closed
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_rate_limited_across_all",
                self.connection_rate_limited_across_all
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "connection_rate_limited_per_ipaddr",
                self.connection_rate_limited_per_ipaddr
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "invalid_stream_size",
                self.invalid_stream_size.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "staked_packets_sent_for_batching",
                self.total_staked_packets_sent_for_batching
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "unstaked_packets_sent_for_batching",
                self.total_unstaked_packets_sent_for_batching
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "packets_sent_to_consumer",
                self.total_packets_sent_to_consumer
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "bytes_sent_to_consumer",
                self.total_bytes_sent_to_consumer.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "chunks_processed_by_batcher",
                self.total_chunks_processed_by_batcher
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "staked_chunks_received",
                self.total_staked_chunks_received.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "unstaked_chunks_received",
                self.total_unstaked_chunks_received
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "total_handle_chunk_to_packet_send_err",
                self.total_handle_chunk_to_packet_send_err
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "total_handle_chunk_to_packet_send_full_err",
                self.total_handle_chunk_to_packet_send_full_err
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "total_handle_chunk_to_packet_send_disconnected_err",
                self.total_handle_chunk_to_packet_send_disconnected_err
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "packet_batch_empty",
                self.total_packet_batches_none.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "stream_read_errors",
                self.total_stream_read_errors.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "stream_read_timeouts",
                self.total_stream_read_timeouts.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "throttled_streams",
                self.throttled_streams.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "stream_load_ema",
                self.stream_load_ema.load(Ordering::Relaxed),
                i64
            ),
            (
                "stream_load_ema_overflow",
                self.stream_load_ema_overflow.load(Ordering::Relaxed),
                i64
            ),
            (
                "stream_load_capacity_overflow",
                self.stream_load_capacity_overflow.load(Ordering::Relaxed),
                i64
            ),
            (
                "throttled_unstaked_streams",
                self.throttled_unstaked_streams.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "throttled_staked_streams",
                self.throttled_staked_streams.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "process_sampled_packets_us_90pct",
                process_sampled_packets_us_hist
                    .percentile(90.0)
                    .unwrap_or(0),
                i64
            ),
            (
                "process_sampled_packets_us_min",
                process_sampled_packets_us_hist.minimum().unwrap_or(0),
                i64
            ),
            (
                "process_sampled_packets_us_max",
                process_sampled_packets_us_hist.maximum().unwrap_or(0),
                i64
            ),
            (
                "process_sampled_packets_us_mean",
                process_sampled_packets_us_hist.mean().unwrap_or(0),
                i64
            ),
            (
                "process_sampled_packets_count",
                process_sampled_packets_us_hist.entries(),
                i64
            ),
            (
                "perf_track_overhead_us",
                self.perf_track_overhead_us.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "outstanding_incoming_connection_attempts",
                self.outstanding_incoming_connection_attempts
                    .load(Ordering::Relaxed),
                i64
            ),
            (
                "total_incoming_connection_attempts",
                self.total_incoming_connection_attempts
                    .load(Ordering::Relaxed),
                i64
            ),
            (
                "quic_endpoints_count",
                self.quic_endpoints_count.load(Ordering::Relaxed),
                i64
            ),
            (
                "open_connections",
                self.open_connections.load(Ordering::Relaxed),
                i64
            ),
            (
                "peak_open_staked_connections",
                self.peak_open_staked_connections.swap(
                    self.open_staked_connections.load(Ordering::Relaxed),
                    Ordering::Relaxed
                ),
                i64
            ),
            (
                "peak_open_unstaked_connections",
                self.peak_open_unstaked_connections.swap(
                    self.open_unstaked_connections.load(Ordering::Relaxed),
                    Ordering::Relaxed
                ),
                i64
            ),
            (
                "refused_connections_too_many_open_connections",
                self.refused_connections_too_many_open_connections
                    .swap(0, Ordering::Relaxed),
                i64
            ),
        );
    }
}

#[derive(Clone)]
pub struct QuicStreamerConfig {
    pub max_connections_per_ipaddr_per_min: u64,
    pub wait_for_chunk_timeout: Duration,
    pub num_threads: NonZeroUsize,
}

#[derive(Clone)]
pub struct SwQosQuicStreamerConfig {
    pub quic_streamer_config: QuicStreamerConfig,
    pub qos_config: SwQosConfig,
}

#[derive(Clone)]
pub struct SimpleQosQuicStreamerConfig {
    pub quic_streamer_config: QuicStreamerConfig,
    pub qos_config: SimpleQosConfig,
}

impl Default for QuicStreamerConfig {
    fn default() -> Self {
        Self {
            max_connections_per_ipaddr_per_min: DEFAULT_MAX_CONNECTIONS_PER_IPADDR_PER_MINUTE,
            wait_for_chunk_timeout: DEFAULT_WAIT_FOR_CHUNK_TIMEOUT,
            num_threads: NonZeroUsize::new(num_cpus::get().min(1)).expect("1 is non-zero"),
        }
    }
}

impl QuicStreamerConfig {
    #[cfg(feature = "dev-context-only-utils")]
    pub const DEFAULT_NUM_SERVER_THREADS_FOR_TEST: NonZeroUsize = NonZeroUsize::new(8).unwrap();

    #[cfg(feature = "dev-context-only-utils")]
    pub fn default_for_tests() -> Self {
        Self {
            num_threads: Self::DEFAULT_NUM_SERVER_THREADS_FOR_TEST,
            ..Self::default()
        }
    }
}

/// Generic function to spawn a tokio runtime with a QUIC server
/// Generic over QoS implementation
fn spawn_runtime_and_server<Q, C>(
    thread_name: &'static str,
    metrics_name: &'static str,
    stats: Arc<StreamerStats>,
    sockets: impl IntoIterator<Item = UdpSocket>,
    keypair: &Keypair,
    packet_sender: Sender<PacketBatch>,
    quic_server_params: QuicStreamerConfig,
    qos: Arc<Q>,
    cancel: CancellationToken,
) -> Result<SpawnServerResult, QuicServerError>
where
    Q: QosController<C> + Send + Sync + 'static,
    C: ConnectionContext + Send + Sync + 'static,
{
    let runtime = rt(format!("{thread_name}Rt"), quic_server_params.num_threads);
    let result = {
        let _guard = runtime.enter();
        crate::nonblocking::quic::spawn_server(
            metrics_name,
            stats,
            sockets,
            keypair,
            packet_sender,
            quic_server_params,
            qos,
            cancel,
        )
    }?;
    let handle = thread::Builder::new()
        .name(thread_name.into())
        .spawn(move || {
            if let Err(e) = runtime.block_on(result.thread) {
                warn!("error from runtime.block_on: {e:?}");
            }
        })
        .unwrap();
    let updater = EndpointKeyUpdater {
        endpoints: result.endpoints.clone(),
    };
    Ok(SpawnServerResult {
        endpoints: result.endpoints,
        thread: handle,
        key_updater: Arc::new(updater),
    })
}

/// Spawns a tokio runtime and a streamer instance inside it.
/// Uses Stake Weighted QoS
pub fn spawn_stake_wighted_qos_server(
    thread_name: &'static str,
    metrics_name: &'static str,
    sockets: impl IntoIterator<Item = UdpSocket>,
    keypair: &Keypair,
    packet_sender: Sender<PacketBatch>,
    staked_nodes: Arc<RwLock<StakedNodes>>,
    quic_server_params: QuicStreamerConfig,
    qos_config: SwQosConfig,
    cancel: CancellationToken,
) -> Result<SpawnServerResult, QuicServerError> {
    let stats = Arc::<StreamerStats>::default();
    let swqos = Arc::new(SwQos::new(
        qos_config,
        stats.clone(),
        staked_nodes,
        cancel.clone(),
    ));
    spawn_runtime_and_server(
        thread_name,
        metrics_name,
        stats,
        sockets,
        keypair,
        packet_sender,
        quic_server_params,
        swqos,
        cancel,
    )
}

/// Spawns a tokio runtime and a streamer instance inside it.
pub fn spawn_simple_qos_server(
    thread_name: &'static str,
    metrics_name: &'static str,
    sockets: impl IntoIterator<Item = UdpSocket>,
    keypair: &Keypair,
    packet_sender: Sender<PacketBatch>,
    staked_nodes: Arc<RwLock<StakedNodes>>,
    quic_server_params: QuicStreamerConfig,
    qos_config: SimpleQosConfig,
    cancel: CancellationToken,
) -> Result<SpawnServerResult, QuicServerError> {
    let stats = Arc::<StreamerStats>::default();
    let simple_qos = Arc::new(SimpleQos::new(
        qos_config,
        stats.clone(),
        staked_nodes,
        cancel.clone(),
    ));

    spawn_runtime_and_server(
        thread_name,
        metrics_name,
        stats,
        sockets,
        keypair,
        packet_sender,
        quic_server_params,
        simple_qos,
        cancel,
    )
}

#[cfg(test)]
mod test {
    use {
        super::*,
        crate::nonblocking::{
            quic::test::*,
            testing_utilities::{check_multiple_streams, make_client_endpoint},
        },
        crossbeam_channel::{unbounded, Receiver},
        solana_net_utils::sockets::bind_to_localhost_unique,
        solana_pubkey::Pubkey,
        solana_signer::Signer,
        std::{collections::HashMap, net::SocketAddr, time::Instant},
        tokio::time::sleep,
    };

    fn rt_for_test() -> Runtime {
        rt(
            "solQuicTestRt".to_string(),
            QuicStreamerConfig::DEFAULT_NUM_SERVER_THREADS_FOR_TEST,
        )
    }

    fn setup_simple_qos_quic_server(
        server_params: SimpleQosQuicStreamerConfig,
        staked_nodes: Arc<RwLock<StakedNodes>>,
    ) -> (
        std::thread::JoinHandle<()>,
        crossbeam_channel::Receiver<PacketBatch>,
        SocketAddr,
        CancellationToken,
    ) {
        let s = bind_to_localhost_unique().expect("should bind");
        let (sender, receiver) = unbounded();
        let keypair = Keypair::new();
        let server_address = s.local_addr().unwrap();
        let cancel = CancellationToken::new();
        let SpawnServerResult {
            endpoints: _,
            thread: t,
            key_updater: _,
        } = spawn_simple_qos_server(
            "solQuicTest",
            "quic_streamer_test",
            [s],
            &keypair,
            sender,
            staked_nodes,
            server_params.quic_streamer_config,
            server_params.qos_config,
            cancel.clone(),
        )
        .unwrap();
        (t, receiver, server_address, cancel)
    }

    fn setup_swqos_quic_server() -> (
        std::thread::JoinHandle<()>,
        crossbeam_channel::Receiver<PacketBatch>,
        SocketAddr,
        CancellationToken,
    ) {
        let staked_nodes = Arc::new(RwLock::new(StakedNodes::default()));

        let server_params = QuicStreamerConfig::default_for_tests();
        let s = bind_to_localhost_unique().expect("should bind");
        let (sender, receiver) = unbounded();
        let keypair = Keypair::new();
        let server_address = s.local_addr().unwrap();
        let cancel = CancellationToken::new();
        let SpawnServerResult {
            endpoints: _,
            thread: t,
            key_updater: _,
        } = spawn_stake_wighted_qos_server(
            "solQuicTest",
            "quic_streamer_test",
            [s],
            &keypair,
            sender,
            staked_nodes,
            server_params,
            SwQosConfig::default_for_tests(),
            cancel.clone(),
        )
        .unwrap();
        (t, receiver, server_address, cancel)
    }

    #[test]
    fn test_quic_server_exit() {
        let (t, _receiver, _server_address, cancel) = setup_swqos_quic_server();
        cancel.cancel();
        t.join().unwrap();
    }

    #[test]
    fn test_quic_timeout() {
        agave_logger::setup();
        let (t, receiver, server_address, cancel) = setup_swqos_quic_server();
        let runtime = rt_for_test();
        runtime.block_on(check_timeout(receiver, server_address));
        cancel.cancel();
        t.join().unwrap();
    }

    #[test]
    fn test_quic_server_block_multiple_connections() {
        agave_logger::setup();
        let (t, _receiver, server_address, cancel) = setup_swqos_quic_server();

        let runtime = rt_for_test();
        runtime.block_on(check_block_multiple_connections(server_address));
        cancel.cancel();
        t.join().unwrap();
    }

    #[test]
    fn test_quic_server_multiple_streams() {
        agave_logger::setup();
        let s = bind_to_localhost_unique().expect("should bind");
        let (sender, receiver) = unbounded();
        let keypair = Keypair::new();
        let server_address = s.local_addr().unwrap();
        let staked_nodes = Arc::new(RwLock::new(StakedNodes::default()));
        let cancel = CancellationToken::new();
        let SpawnServerResult {
            endpoints: _,
            thread: t,
            key_updater: _,
        } = spawn_stake_wighted_qos_server(
            "solQuicTest",
            "quic_streamer_test",
            [s],
            &keypair,
            sender,
            staked_nodes,
            QuicStreamerConfig {
                ..QuicStreamerConfig::default_for_tests()
            },
            SwQosConfig {
                max_connections_per_unstaked_peer: 2,
                ..Default::default()
            },
            cancel.clone(),
        )
        .unwrap();

        let runtime = rt_for_test();
        runtime.block_on(check_multiple_streams(receiver, server_address, None));
        cancel.cancel();
        t.join().unwrap();
    }

    #[test]
    fn test_quic_server_multiple_writes() {
        agave_logger::setup();
        let (t, receiver, server_address, cancel) = setup_swqos_quic_server();

        let runtime = rt_for_test();
        runtime.block_on(check_multiple_writes(receiver, server_address, None));
        cancel.cancel();
        t.join().unwrap();
    }

    #[test]
    fn test_quic_server_multiple_packets_with_simple_qos() {
        // Send multiple writes from a staked node with simple QoS mode
        // and verify pubkey is sent along with packets.
        agave_logger::setup();
        let client_keypair = Keypair::new();
        let rich_node_keypair = Keypair::new();

        let stakes = HashMap::from([
            (client_keypair.pubkey(), 1_000), // very small staked node
            (rich_node_keypair.pubkey(), 1_000_000_000),
        ]);
        let staked_nodes = StakedNodes::new(
            Arc::new(stakes),
            HashMap::<Pubkey, u64>::default(), // overrides
        );

        let server_params = QuicStreamerConfig::default_for_tests();
        let qos_config = SimpleQosConfig {
            max_connections_per_peer: 2,
            max_streams_per_second: 20,
            ..Default::default()
        };
        let server_params = SimpleQosQuicStreamerConfig {
            quic_streamer_config: server_params,
            qos_config,
        };
        let (t, receiver, server_address, cancel) =
            setup_simple_qos_quic_server(server_params, Arc::new(RwLock::new(staked_nodes)));

        let runtime = rt_for_test();
        let num_expected_packets = 20;

        runtime.block_on(check_multiple_packets_with_client_id(
            receiver,
            server_address,
            Some(&client_keypair),
            Some(&rich_node_keypair),
            num_expected_packets,
        ));
        cancel.cancel();
        t.join().unwrap();
    }

    #[test]
    fn test_quic_server_unstaked_node_connect_failure() {
        agave_logger::setup();
        let s = bind_to_localhost_unique().expect("should bind");
        let (sender, _) = unbounded();
        let keypair = Keypair::new();
        let server_address = s.local_addr().unwrap();
        let staked_nodes = Arc::new(RwLock::new(StakedNodes::default()));
        let cancel = CancellationToken::new();
        let SpawnServerResult {
            endpoints: _,
            thread: t,
            key_updater: _,
        } = spawn_stake_wighted_qos_server(
            "solQuicTest",
            "quic_streamer_test",
            [s],
            &keypair,
            sender,
            staked_nodes,
            QuicStreamerConfig {
                ..QuicStreamerConfig::default_for_tests()
            },
            SwQosConfig {
                max_unstaked_connections: 0,
                ..Default::default()
            },
            cancel.clone(),
        )
        .unwrap();

        let runtime = rt_for_test();
        runtime.block_on(check_unstaked_node_connect_failure(server_address));
        cancel.cancel();
        t.join().unwrap();
    }

    async fn check_multiple_packets_with_client_id(
        receiver: Receiver<PacketBatch>,
        server_address: SocketAddr,
        client_keypair1: Option<&Keypair>,
        client_keypair2: Option<&Keypair>,
        num_expected_packets: usize,
    ) {
        let conn1 = Arc::new(make_client_endpoint(&server_address, client_keypair1).await);
        let conn2 = Arc::new(make_client_endpoint(&server_address, client_keypair2).await);

        debug!(
            "Connections established: {} and {}",
            conn1.remote_address(),
            conn2.remote_address(),
        );

        let expected_client_pubkey_1 = client_keypair1.map(|kp| kp.pubkey());
        let expected_client_pubkey_2 = client_keypair2.map(|kp| kp.pubkey());

        let mut num_packets_sent = 0;
        for i in 0..num_expected_packets {
            debug!("Sending stream pair {i}");
            let c1 = conn1.clone();
            let c2 = conn2.clone();

            let mut s1 = c1.open_uni().await.unwrap();
            let mut s2 = c2.open_uni().await.unwrap();

            s1.write_all(&[0u8]).await.unwrap();
            s1.finish().unwrap();
            debug!("Stream {i}.1 sent and finished");

            if i < num_expected_packets - 1 {
                s2.write_all(&[1u8]).await.unwrap();
                s2.finish().unwrap();
                debug!("Stream {i}.2 sent and finished");
                num_packets_sent += 2;
            } else {
                num_packets_sent += 1;
            }
        }

        debug!("All streams sent, expecting {num_packets_sent} packets with client ID");

        let now = Instant::now();
        let mut total_packets = 0;
        let mut iterations = 0;

        while now.elapsed().as_secs() < 2 {
            iterations += 1;
            match receiver.try_recv() {
                Ok(packet_batch) => {
                    debug!("Received packet batch (iteration {iterations})");

                    // Verify we get the client pubkey
                    match &packet_batch {
                        PacketBatch::Bytes(_) => {
                            panic!("Expected PacketBatch::Simple but got PacketBatch::Bytes");
                        }
                        PacketBatch::Pinned(_) => {
                            panic!("Expected PacketBatch::Simple but got PacketBatch::Pinned");
                        }
                        PacketBatch::Single(packet) => {
                            if *packet.data(0).unwrap() == 0u8 {
                                debug!("Packet from stream with client 1");
                                assert_eq!(packet.meta().remote_pubkey(), expected_client_pubkey_1);
                            } else if *packet.data(0).unwrap() == 1u8 {
                                debug!("Packet from stream with client 2");
                                assert_eq!(packet.meta().remote_pubkey(), expected_client_pubkey_2);
                            } else {
                                panic!("Unexpected data in packet: {:?}", packet.data(0));
                            }
                            total_packets += 1;
                        }
                    }
                }
                Err(e) => {
                    if iterations % 10 == 0 {
                        debug!("No packets yet (iteration {iterations}): {e:?}");
                    }
                    sleep(Duration::from_millis(100)).await;
                }
            }

            if total_packets >= num_packets_sent {
                debug!("Received all expected packets with client ID!");
                break;
            }

            if iterations % 50 == 0 {
                debug!(
                    "Still waiting... received {total_packets}/{num_packets_sent} packets after \
                     {iterations} iterations",
                );
            }
        }

        debug!(
            "Final: received {total_packets}/{num_packets_sent} packets in {iterations} iterations",
        );

        assert!(
            total_packets >= num_packets_sent,
            "Expected at least {num_packets_sent} packets with client ID, got {total_packets}",
        );
    }
}
