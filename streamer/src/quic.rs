use {
    crate::{
        nonblocking::quic::{ConnectionPeerType, ALPN_TPU_PROTOCOL_ID},
        streamer::StakedNodes,
        tls_certificates::new_self_signed_tls_certificate,
    },
    crossbeam_channel::Sender,
    pem::Pem,
    quinn::{Endpoint, IdleTimeout, ServerConfig},
    rustls::{server::ClientCertVerified, Certificate, DistinguishedName},
    solana_perf::packet::PacketBatch,
    solana_sdk::{
        packet::PACKET_DATA_SIZE,
        pubkey::Pubkey,
        quic::{QUIC_MAX_TIMEOUT, QUIC_MAX_UNSTAKED_CONCURRENT_STREAMS},
        signature::Keypair,
    },
    std::{
        collections::HashMap,
        net::{IpAddr, UdpSocket},
        sync::{
            atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
            Arc, RwLock,
        },
        thread,
        time::{Duration, Instant, SystemTime},
    },
    tokio::{runtime::Runtime, sync::Mutex},
};

pub const MAX_STAKED_CONNECTIONS: usize = 2000;
pub const MAX_UNSTAKED_CONNECTIONS: usize = 500;

pub struct SkipClientVerification;

impl SkipClientVerification {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl rustls::server::ClientCertVerifier for SkipClientVerification {
    fn client_auth_root_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &Certificate,
        _intermediates: &[Certificate],
        _now: SystemTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(rustls::server::ClientCertVerified::assertion())
    }
}

/// Returns default server configuration along with its PEM certificate chain.
#[allow(clippy::field_reassign_with_default)] // https://github.com/rust-lang/rust-clippy/issues/6527
pub(crate) fn configure_server(
    identity_keypair: &Keypair,
    gossip_host: IpAddr,
) -> Result<(ServerConfig, String), QuicServerError> {
    let (cert, priv_key) = new_self_signed_tls_certificate(identity_keypair, gossip_host)?;
    let cert_chain_pem_parts = vec![Pem {
        tag: "CERTIFICATE".to_string(),
        contents: cert.0.clone(),
    }];
    let cert_chain_pem = pem::encode_many(&cert_chain_pem_parts);

    let mut server_tls_config = rustls::ServerConfig::builder()
        .with_safe_defaults()
        .with_client_cert_verifier(SkipClientVerification::new())
        .with_single_cert(vec![cert], priv_key)?;
    server_tls_config.alpn_protocols = vec![ALPN_TPU_PROTOCOL_ID.to_vec()];

    let mut server_config = ServerConfig::with_crypto(Arc::new(server_tls_config));
    server_config
        .concurrent_connections(2 * (MAX_STAKED_CONNECTIONS + MAX_UNSTAKED_CONNECTIONS) as u32);
    let config = Arc::get_mut(&mut server_config.transport).unwrap();

    // QUIC_MAX_CONCURRENT_STREAMS doubled, which was found to improve reliability
    const MAX_CONCURRENT_UNI_STREAMS: u32 =
        (QUIC_MAX_UNSTAKED_CONCURRENT_STREAMS.saturating_mul(2)) as u32;
    config.max_concurrent_uni_streams(MAX_CONCURRENT_UNI_STREAMS.into());
    config.stream_receive_window((PACKET_DATA_SIZE as u32).into());
    config.receive_window(
        (PACKET_DATA_SIZE as u32)
            .saturating_mul(MAX_CONCURRENT_UNI_STREAMS)
            .into(),
    );
    let timeout = IdleTimeout::try_from(QUIC_MAX_TIMEOUT).unwrap();
    config.max_idle_timeout(Some(timeout));

    // disable bidi & datagrams
    const MAX_CONCURRENT_BIDI_STREAMS: u32 = 0;
    config.max_concurrent_bidi_streams(MAX_CONCURRENT_BIDI_STREAMS.into());
    config.datagram_receive_buffer_size(None);

    Ok((server_config, cert_chain_pem))
}

fn rt() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .thread_name("quic-server")
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap()
}

#[derive(thiserror::Error, Debug)]
pub enum QuicServerError {
    #[error("Endpoint creation failed: {0}")]
    EndpointFailed(std::io::Error),
    #[error("Certificate error: {0}")]
    CertificateError(#[from] rcgen::RcgenError),
    #[error("TLS error: {0}")]
    TlsError(#[from] rustls::Error),
}

#[derive(Default)]
pub struct StreamStats {
    pub(crate) total_connections: AtomicUsize,
    pub(crate) total_new_connections: AtomicUsize,
    pub(crate) total_streams: AtomicUsize,
    pub(crate) total_new_streams: AtomicUsize,
    pub(crate) total_invalid_chunks: AtomicUsize,
    pub(crate) total_invalid_chunk_size: AtomicUsize,
    pub(crate) total_packets_allocated: AtomicUsize,
    pub(crate) total_packet_batches_allocated: AtomicUsize,
    pub(crate) total_chunks_received: AtomicUsize,
    pub(crate) total_staked_chunks_received: AtomicUsize,
    pub(crate) total_unstaked_chunks_received: AtomicUsize,
    pub(crate) total_packet_batch_send_err: AtomicUsize,
    pub(crate) total_handle_chunk_to_packet_batcher_send_err: AtomicUsize,
    pub(crate) total_packet_batches_sent: AtomicUsize,
    pub(crate) total_packet_batches_none: AtomicUsize,
    pub(crate) total_packets_sent_for_batching: AtomicUsize,
    pub(crate) total_bytes_sent_for_batching: AtomicUsize,
    pub(crate) total_chunks_sent_for_batching: AtomicUsize,
    pub(crate) total_packets_sent_to_consumer: AtomicUsize,
    pub(crate) total_bytes_sent_to_consumer: AtomicUsize,
    pub(crate) total_chunks_processed_by_batcher: AtomicUsize,
    pub(crate) total_stream_read_errors: AtomicUsize,
    pub(crate) total_stream_read_timeouts: AtomicUsize,
    // The total incoming connections received which have not gone through full handshake yet
    pub(crate) incoming_connections_received: AtomicUsize,
    pub(crate) num_evictions: AtomicUsize,
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
    pub(crate) connection_throttled: AtomicUsize,
    pub(crate) throttled_streams: AtomicUsize,
    pub(crate) process_sampled_packets_us_hist: Mutex<histogram::Histogram>,
    pub(crate) perf_track_overhead_us: AtomicU64,
    pub(crate) total_staked_packets_sent_for_batching: AtomicUsize,
    pub(crate) total_unstaked_packets_sent_for_batching: AtomicUsize,
    pub(crate) throttled_staked_streams: AtomicUsize,
    pub(crate) throttled_unstaked_streams: AtomicUsize,
    pub(crate) received_streams: AtomicUsize,
    pub(crate) received_unstaked_streams: AtomicUsize,
    pub(crate) received_staked_streams: AtomicUsize,
    pub(crate) peer_stats: PeerStatsRecorder,
}

/// Stats per peer
pub struct PeerStats {
    pub(crate) peer_type: ConnectionPeerType,
    pub(crate) stakes: AtomicUsize,
    pub(crate) received_streams: AtomicUsize,
    pub(crate) throttled_streams: AtomicUsize,
    pub(crate) packets_sent_for_batching: AtomicUsize,
    pub(crate) bytes_sent_for_batching: AtomicUsize,
    pub(crate) stream_read_errors: AtomicUsize,
    pub(crate) stream_read_timeouts: AtomicUsize,
    pub(crate) connection_count: AtomicUsize,
    pub(crate) start_time: Instant,
    pub(crate) end_time: Instant,
}

impl Default for PeerStats {
    fn default() -> Self {
        Self {
            peer_type: ConnectionPeerType::default(),
            stakes: AtomicUsize::default(),
            received_streams: AtomicUsize::default(),
            throttled_streams: AtomicUsize::default(),
            packets_sent_for_batching: AtomicUsize::default(),
            bytes_sent_for_batching: AtomicUsize::default(),
            stream_read_errors: AtomicUsize::default(),
            stream_read_timeouts: AtomicUsize::default(),
            connection_count: AtomicUsize::default(),
            start_time: Instant::now(),
            end_time: Instant::now(),
        }
    }
}

impl Clone for PeerStats {
    fn clone(&self) -> Self {
        Self {
            received_streams: AtomicUsize::new(self.received_streams.load(Ordering::Relaxed)),
            throttled_streams: AtomicUsize::new(self.throttled_streams.load(Ordering::Relaxed)),
            peer_type: self.peer_type,
            connection_count: AtomicUsize::new(self.connection_count.load(Ordering::Relaxed)),
            stakes: AtomicUsize::new(self.stakes.load(Ordering::Relaxed)),
            stream_read_errors: AtomicUsize::new(self.stream_read_errors.load(Ordering::Relaxed)),
            stream_read_timeouts: AtomicUsize::new(
                self.stream_read_timeouts.load(Ordering::Relaxed),
            ),
            packets_sent_for_batching: AtomicUsize::new(
                self.packets_sent_for_batching.load(Ordering::Relaxed),
            ),
            bytes_sent_for_batching: AtomicUsize::new(
                self.bytes_sent_for_batching.load(Ordering::Relaxed),
            ),
            start_time: self.start_time,
            end_time: self.end_time,
        }
    }
}

impl PeerStats {
    pub fn new(peer_type: ConnectionPeerType, stakes: u64, connection_count: usize) -> Self {
        Self {
            peer_type,
            stakes: AtomicUsize::new(stakes as usize),
            connection_count: AtomicUsize::new(connection_count),
            ..Default::default()
        }
    }
}

#[derive(Default)]
pub struct PeerStatsRecorder {
    pub(crate) stats: Mutex<HashMap<Pubkey, PeerStats>>,
}

impl StreamStats {
    pub async fn report(&self, name: &'static str) {
        let mut process_sampled_packets_us_hist = self.process_sampled_packets_us_hist.lock().await;
        datapoint_info!(
            name,
            (
                "active_connections",
                self.total_connections.load(Ordering::Relaxed),
                i64
            ),
            (
                "active_streams",
                self.total_streams.load(Ordering::Relaxed),
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
                "evictions",
                self.num_evictions.swap(0, Ordering::Relaxed),
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
                "connection_throttled",
                self.connection_throttled
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "invalid_chunk",
                self.total_invalid_chunks.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "invalid_chunk_size",
                self.total_invalid_chunk_size.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "packets_allocated",
                self.total_packets_allocated.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "packet_batches_allocated",
                self.total_packet_batches_allocated
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "packets_sent_for_batching",
                self.total_packets_sent_for_batching
                    .swap(0, Ordering::Relaxed),
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
                "bytes_sent_for_batching",
                self.total_bytes_sent_for_batching
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "chunks_sent_for_batching",
                self.total_chunks_sent_for_batching
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
                "chunks_received",
                self.total_chunks_received.swap(0, Ordering::Relaxed),
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
                "packet_batch_send_error",
                self.total_packet_batch_send_err.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "handle_chunk_to_packet_batcher_send_error",
                self.total_handle_chunk_to_packet_batcher_send_err
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "packet_batches_sent",
                self.total_packet_batches_sent.swap(0, Ordering::Relaxed),
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
                "incoming_connections_received",
                self.incoming_connections_received
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "throttled_streams",
                self.throttled_streams.swap(0, Ordering::Relaxed),
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
                "perf_track_overhead_us",
                self.perf_track_overhead_us.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "received_streams",
                self.received_streams.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "received_unstaked_streams",
                self.received_unstaked_streams.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "received_staked_streams",
                self.received_staked_streams.swap(0, Ordering::Relaxed),
                i64
            ),
        );
        process_sampled_packets_us_hist.clear();
        self.report_peer_stats().await;
    }

    async fn report_peer_stats(&self) {
        let map = {
            let mut stats = self.peer_stats.stats.lock().await;
            let new_map = HashMap::<Pubkey, PeerStats>::default();
            std::mem::replace(&mut *stats, new_map)
        };
        // now we can report the stats without holding the lock
        for (key, stats) in map {
            info!(
                "STREAMER_PEER_STATS: node={key}, type={:?}, stakes={}, received_streams={}, throttled_streams={}, packets_sent={}, bytes_sent={}, read_errors={}, read_timeouts={}, connection_count={} duration_us={}",
                stats.peer_type,
                stats.stakes.load(Ordering::Relaxed),
                stats.received_streams.load(Ordering::Relaxed),
                stats.throttled_streams.load(Ordering::Relaxed),
                stats.packets_sent_for_batching.load(Ordering::Relaxed),
                stats.bytes_sent_for_batching.load(Ordering::Relaxed),
                stats.stream_read_errors.load(Ordering::Relaxed),
                stats.stream_read_timeouts.load(Ordering::Relaxed),
                stats.connection_count.load(Ordering::Relaxed),
                stats.end_time.duration_since(stats.start_time).as_micros()
            );
        }
    }

    /// Update the peer stat for a peer given its key
    pub async fn update_peer_stats(&self, pubkey: &Pubkey, peer_stat: &PeerStats) {
        let mut stats = self.peer_stats.stats.lock().await;
        stats
            .entry(*pubkey)
            .and_modify(|stat| {
                stat.peer_type = peer_stat.peer_type;
                stat.stakes
                    .store(peer_stat.stakes.load(Ordering::Relaxed), Ordering::Relaxed);
                stat.received_streams.fetch_add(
                    peer_stat.received_streams.swap(0, Ordering::Relaxed),
                    Ordering::Relaxed,
                );
                stat.throttled_streams.fetch_add(
                    peer_stat.throttled_streams.swap(0, Ordering::Relaxed),
                    Ordering::Relaxed,
                );
                stat.packets_sent_for_batching.fetch_add(
                    peer_stat
                        .packets_sent_for_batching
                        .swap(0, Ordering::Relaxed),
                    Ordering::Relaxed,
                );
                stat.bytes_sent_for_batching.fetch_add(
                    peer_stat.bytes_sent_for_batching.swap(0, Ordering::Relaxed),
                    Ordering::Relaxed,
                );
                stat.stream_read_errors.fetch_add(
                    peer_stat.stream_read_errors.swap(0, Ordering::Relaxed),
                    Ordering::Relaxed,
                );
                stat.stream_read_timeouts.fetch_add(
                    peer_stat.stream_read_timeouts.swap(0, Ordering::Relaxed),
                    Ordering::Relaxed,
                );
                stat.connection_count.store(
                    peer_stat.connection_count.load(Ordering::Relaxed),
                    Ordering::Relaxed,
                );
                stat.end_time = Instant::now();
            })
            .or_insert(peer_stat.clone());
    }
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_server(
    name: &'static str,
    sock: UdpSocket,
    keypair: &Keypair,
    gossip_host: IpAddr,
    packet_sender: Sender<PacketBatch>,
    exit: Arc<AtomicBool>,
    max_connections_per_peer: usize,
    staked_nodes: Arc<RwLock<StakedNodes>>,
    max_staked_connections: usize,
    max_unstaked_connections: usize,
    wait_for_chunk_timeout: Duration,
    coalesce: Duration,
) -> Result<(Endpoint, thread::JoinHandle<()>), QuicServerError> {
    let runtime = rt();
    let (endpoint, _stats, task) = {
        let _guard = runtime.enter();
        crate::nonblocking::quic::spawn_server(
            name,
            sock,
            keypair,
            gossip_host,
            packet_sender,
            exit,
            max_connections_per_peer,
            staked_nodes,
            max_staked_connections,
            max_unstaked_connections,
            wait_for_chunk_timeout,
            coalesce,
        )
    }?;
    let handle = thread::Builder::new()
        .name("solQuicServer".into())
        .spawn(move || {
            if let Err(e) = runtime.block_on(task) {
                warn!("error from runtime.block_on: {:?}", e);
            }
        })
        .unwrap();
    Ok((endpoint, handle))
}

#[cfg(test)]
mod test {
    use {
        super::*,
        crate::nonblocking::quic::{test::*, DEFAULT_WAIT_FOR_CHUNK_TIMEOUT},
        crossbeam_channel::unbounded,
        solana_sdk::net::DEFAULT_TPU_COALESCE,
        std::net::SocketAddr,
    };

    fn setup_quic_server() -> (
        std::thread::JoinHandle<()>,
        Arc<AtomicBool>,
        crossbeam_channel::Receiver<PacketBatch>,
        SocketAddr,
    ) {
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        let exit = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = unbounded();
        let keypair = Keypair::new();
        let ip = "127.0.0.1".parse().unwrap();
        let server_address = s.local_addr().unwrap();
        let staked_nodes = Arc::new(RwLock::new(StakedNodes::default()));
        let (_, t) = spawn_server(
            "quic_streamer_test",
            s,
            &keypair,
            ip,
            sender,
            exit.clone(),
            1,
            staked_nodes,
            MAX_STAKED_CONNECTIONS,
            MAX_UNSTAKED_CONNECTIONS,
            DEFAULT_WAIT_FOR_CHUNK_TIMEOUT,
            DEFAULT_TPU_COALESCE,
        )
        .unwrap();
        (t, exit, receiver, server_address)
    }

    #[test]
    fn test_quic_server_exit() {
        let (t, exit, _receiver, _server_address) = setup_quic_server();
        exit.store(true, Ordering::Relaxed);
        t.join().unwrap();
    }

    #[test]
    fn test_quic_timeout() {
        solana_logger::setup();
        let (t, exit, receiver, server_address) = setup_quic_server();
        let runtime = rt();
        runtime.block_on(check_timeout(receiver, server_address));
        exit.store(true, Ordering::Relaxed);
        t.join().unwrap();
    }

    #[test]
    fn test_quic_server_block_multiple_connections() {
        solana_logger::setup();
        let (t, exit, _receiver, server_address) = setup_quic_server();

        let runtime = rt();
        runtime.block_on(check_block_multiple_connections(server_address));
        exit.store(true, Ordering::Relaxed);
        t.join().unwrap();
    }

    #[test]
    fn test_quic_server_multiple_streams() {
        solana_logger::setup();
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        let exit = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = unbounded();
        let keypair = Keypair::new();
        let ip = "127.0.0.1".parse().unwrap();
        let server_address = s.local_addr().unwrap();
        let staked_nodes = Arc::new(RwLock::new(StakedNodes::default()));
        let (_, t) = spawn_server(
            "quic_streamer_test",
            s,
            &keypair,
            ip,
            sender,
            exit.clone(),
            2,
            staked_nodes,
            MAX_STAKED_CONNECTIONS,
            MAX_UNSTAKED_CONNECTIONS,
            DEFAULT_WAIT_FOR_CHUNK_TIMEOUT,
            DEFAULT_TPU_COALESCE,
        )
        .unwrap();

        let runtime = rt();
        runtime.block_on(check_multiple_streams(receiver, server_address));
        exit.store(true, Ordering::Relaxed);
        t.join().unwrap();
    }

    #[test]
    fn test_quic_server_multiple_writes() {
        solana_logger::setup();
        let (t, exit, receiver, server_address) = setup_quic_server();

        let runtime = rt();
        runtime.block_on(check_multiple_writes(receiver, server_address, None));
        exit.store(true, Ordering::Relaxed);
        t.join().unwrap();
    }

    #[test]
    fn test_quic_server_unstaked_node_connect_failure() {
        solana_logger::setup();
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        let exit = Arc::new(AtomicBool::new(false));
        let (sender, _) = unbounded();
        let keypair = Keypair::new();
        let ip = "127.0.0.1".parse().unwrap();
        let server_address = s.local_addr().unwrap();
        let staked_nodes = Arc::new(RwLock::new(StakedNodes::default()));
        let (_, t) = spawn_server(
            "quic_streamer_test",
            s,
            &keypair,
            ip,
            sender,
            exit.clone(),
            1,
            staked_nodes,
            MAX_STAKED_CONNECTIONS,
            0, // Do not allow any connection from unstaked clients/nodes
            DEFAULT_WAIT_FOR_CHUNK_TIMEOUT,
            DEFAULT_TPU_COALESCE,
        )
        .unwrap();

        let runtime = rt();
        runtime.block_on(check_unstaked_node_connect_failure(server_address));
        exit.store(true, Ordering::Relaxed);
        t.join().unwrap();
    }
}
