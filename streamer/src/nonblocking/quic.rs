use {
    crate::{
        nonblocking::{
            connection_rate_limiter::ConnectionRateLimiter,
            qos::{ConnectionContext, OpaqueStreamerCounter, QosController},
        },
        quic::{configure_server, QuicServerError, QuicStreamerConfig, StreamerStats},
        streamer::StakedNodes,
    },
    bytes::{BufMut, Bytes, BytesMut},
    crossbeam_channel::{Sender, TrySendError},
    futures::{stream::FuturesUnordered, Future, StreamExt as _},
    indexmap::map::{Entry, IndexMap},
    quinn::{Accept, Connecting, Connection, Endpoint, EndpointConfig, TokioRuntime},
    rand::{rng, Rng},
    smallvec::SmallVec,
    solana_keypair::Keypair,
    solana_measure::measure::Measure,
    solana_net_utils::token_bucket::TokenBucket,
    solana_packet::{Meta, PACKET_DATA_SIZE},
    solana_perf::packet::{BytesPacket, PacketBatch},
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    solana_tls_utils::get_pubkey_from_tls_certificate,
    solana_transaction_metrics_tracker::signature_if_should_track_packet,
    std::{
        array, fmt,
        iter::repeat_with,
        net::{IpAddr, SocketAddr, UdpSocket},
        pin::Pin,
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc, RwLock,
        },
        task::Poll,
        time::{Duration, Instant},
    },
    tokio::{
        // CAUTION: It's kind of sketch that we're mixing async and sync locks (see the RwLock above).
        // This is done so that sync code can also access the stake table.
        // Make sure we don't hold a sync lock across an await - including the await to
        // lock an async Mutex. This does not happen now and should not happen as long as we
        // don't hold an async Mutex and sync RwLock at the same time (currently true)
        // but if we do, the scope of the RwLock must always be a subset of the async Mutex
        // (i.e. lock order is always async Mutex -> RwLock). Also, be careful not to
        // introduce any other awaits while holding the RwLock.
        select,
        task::JoinHandle,
        time::timeout,
    },
    tokio_util::{sync::CancellationToken, task::TaskTracker},
};

pub const DEFAULT_WAIT_FOR_CHUNK_TIMEOUT: Duration = Duration::from_secs(2);

pub const ALPN_TPU_PROTOCOL_ID: &[u8] = b"solana-tpu";

const CONNECTION_CLOSE_CODE_DROPPED_ENTRY: u32 = 1;
const CONNECTION_CLOSE_REASON_DROPPED_ENTRY: &[u8] = b"dropped";

pub(crate) const CONNECTION_CLOSE_CODE_DISALLOWED: u32 = 2;
pub(crate) const CONNECTION_CLOSE_REASON_DISALLOWED: &[u8] = b"disallowed";

pub(crate) const CONNECTION_CLOSE_CODE_EXCEED_MAX_STREAM_COUNT: u32 = 3;
pub(crate) const CONNECTION_CLOSE_REASON_EXCEED_MAX_STREAM_COUNT: &[u8] =
    b"exceed_max_stream_count";

const CONNECTION_CLOSE_CODE_TOO_MANY: u32 = 4;
const CONNECTION_CLOSE_REASON_TOO_MANY: &[u8] = b"too_many";

const CONNECTION_CLOSE_CODE_INVALID_STREAM: u32 = 5;
const CONNECTION_CLOSE_REASON_INVALID_STREAM: &[u8] = b"invalid_stream";

/// Total new connection counts per second. Heuristically taken from
/// the default staked and unstaked connection limits. Might be adjusted
/// later.
const TOTAL_CONNECTIONS_PER_SECOND: f64 = 2500.0;

/// Max burst of connections above sustained rate to pass through
const MAX_CONNECTION_BURST: u64 = 1000;

/// Timeout for connection handshake. Timer starts once we get Initial from the
/// peer, and is canceled when we get a Handshake packet from them.
const QUIC_CONNECTION_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);

/// Absolute max RTT to allow for a legitimate connection.
/// Enough to cover any non-malicious link on Earth.
pub(crate) const MAX_RTT: Duration = Duration::from_millis(320);
/// Prevent connections from having 0 RTT when RTT is too small,
/// as this would break some BDP calculations and assign zero bandwidth
pub(crate) const MIN_RTT: Duration = Duration::from_millis(2);

// A struct to accumulate the bytes making up
// a packet, along with their offsets, and the
// packet metadata. We use this accumulator to avoid
// multiple copies of the Bytes (when building up
// the Packet and then when copying the Packet into a PacketBatch)
#[derive(Clone)]
struct PacketAccumulator {
    pub meta: Meta,
    pub chunks: SmallVec<[Bytes; 2]>,
    pub start_time: Instant,
}

impl PacketAccumulator {
    fn new(meta: Meta) -> Self {
        Self {
            meta,
            chunks: SmallVec::default(),
            start_time: Instant::now(),
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub enum ConnectionPeerType {
    Unstaked,
    Staked(u64),
}

impl ConnectionPeerType {
    pub(crate) fn is_staked(&self) -> bool {
        matches!(self, ConnectionPeerType::Staked(_))
    }
}

pub struct SpawnNonBlockingServerResult {
    pub endpoints: Vec<Endpoint>,
    pub stats: Arc<StreamerStats>,
    pub thread: JoinHandle<()>,
    pub max_concurrent_connections: usize,
}

/// Spawn a streamer instance in the current tokio runtime.
pub(crate) fn spawn_server<Q, C>(
    name: &'static str,
    stats: Arc<StreamerStats>,
    sockets: impl IntoIterator<Item = UdpSocket>,
    keypair: &Keypair,
    packet_sender: Sender<PacketBatch>,
    quic_server_params: QuicStreamerConfig,
    qos: Arc<Q>,
    cancel: CancellationToken,
) -> Result<SpawnNonBlockingServerResult, QuicServerError>
where
    Q: QosController<C> + Send + Sync + 'static,
    C: ConnectionContext + Send + Sync + 'static,
{
    let sockets: Vec<_> = sockets.into_iter().collect();
    info!("Start {name} quic server on {sockets:?}");
    let (config, _) = configure_server(keypair)?;

    let endpoints = sockets
        .into_iter()
        .map(|sock| {
            Endpoint::new(
                EndpointConfig::default(),
                Some(config.clone()),
                sock,
                Arc::new(TokioRuntime),
            )
            .map_err(QuicServerError::EndpointFailed)
        })
        .collect::<Result<Vec<_>, _>>()?;

    let max_concurrent_connections = qos.max_concurrent_connections();
    let handle = tokio::spawn({
        let endpoints = endpoints.clone();
        let stats = stats.clone();
        async move {
            let tasks = run_server(
                name,
                endpoints.clone(),
                packet_sender,
                stats.clone(),
                quic_server_params,
                cancel,
                qos,
            )
            .await;
            tasks.close();
            tasks.wait().await;
        }
    });

    Ok(SpawnNonBlockingServerResult {
        endpoints,
        stats,
        thread: handle,
        max_concurrent_connections,
    })
}

/// struct ease tracking connections of all stages, so that we do not have to
/// litter the code with open connection tracking. This is added into the
/// connection table as part of the ConnectionEntry. The reference is auto
/// reduced when it is dropped.
pub struct ClientConnectionTracker {
    pub(crate) stats: Arc<StreamerStats>,
}

/// This is required by ConnectionEntry for supporting debug format.
impl fmt::Debug for ClientConnectionTracker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamerClientConnection")
            .field(
                "open_connections:",
                &self.stats.open_connections.load(Ordering::Relaxed),
            )
            .finish()
    }
}

impl Drop for ClientConnectionTracker {
    /// When this is dropped, reduce the open connection count.
    fn drop(&mut self) {
        self.stats.open_connections.fetch_sub(1, Ordering::Relaxed);
    }
}

impl ClientConnectionTracker {
    /// Check the max_concurrent_connections limit and if it is within the limit
    /// create ClientConnectionTracker and increment open connection count. Otherwise returns Err
    fn new(stats: Arc<StreamerStats>, max_concurrent_connections: usize) -> Result<Self, ()> {
        let open_connections = stats.open_connections.fetch_add(1, Ordering::Relaxed);
        if open_connections >= max_concurrent_connections {
            stats.open_connections.fetch_sub(1, Ordering::Relaxed);
            debug!(
                "There are too many concurrent connections opened already: open: \
                 {open_connections}, max: {max_concurrent_connections}"
            );
            return Err(());
        }

        Ok(Self { stats })
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_server<Q, C>(
    name: &'static str,
    endpoints: Vec<Endpoint>,
    packet_batch_sender: Sender<PacketBatch>,
    stats: Arc<StreamerStats>,
    quic_server_params: QuicStreamerConfig,
    cancel: CancellationToken,
    qos: Arc<Q>,
) -> TaskTracker
where
    Q: QosController<C> + Send + Sync + 'static,
    C: ConnectionContext + Send + Sync + 'static,
{
    let quic_server_params = Arc::new(quic_server_params);
    let rate_limiter = Arc::new(ConnectionRateLimiter::new(
        quic_server_params.max_connections_per_ipaddr_per_min,
        // allow for 10x burst to make sure we can accommodate legitimate
        // bursts from container environments running multiple pods on same IP
        quic_server_params.max_connections_per_ipaddr_per_min * 10,
        quic_server_params.num_threads.get() * 2,
    ));
    let overall_connection_rate_limiter = Arc::new(TokenBucket::new(
        MAX_CONNECTION_BURST,
        MAX_CONNECTION_BURST,
        TOTAL_CONNECTIONS_PER_SECOND,
    ));

    const WAIT_FOR_CONNECTION_TIMEOUT: Duration = Duration::from_secs(1);
    debug!("spawn quic server");
    let mut last_datapoint = Instant::now();
    stats
        .quic_endpoints_count
        .store(endpoints.len(), Ordering::Relaxed);

    let mut accepts = endpoints
        .iter()
        .enumerate()
        .map(|(i, incoming)| {
            Box::pin(EndpointAccept {
                accept: incoming.accept(),
                endpoint: i,
            })
        })
        .collect::<FuturesUnordered<_>>();

    let tasks = TaskTracker::new();
    loop {
        let timeout_connection = select! {
            ready = accepts.next() => {
                if let Some((connecting, i)) = ready {
                    accepts.push(
                        Box::pin(EndpointAccept {
                            accept: endpoints[i].accept(),
                            endpoint: i,
                        }
                    ));
                    Ok(connecting)
                } else {
                    // we can't really get here - we never poll an empty FuturesUnordered
                    continue
                }
            }
            _ = tokio::time::sleep(WAIT_FOR_CONNECTION_TIMEOUT) => {
                Err(())
            }
            _ = cancel.cancelled() => break,
        };

        if last_datapoint.elapsed().as_secs() >= 5 {
            stats.report(name);
            last_datapoint = Instant::now();
        }

        if let Ok(Some(incoming)) = timeout_connection {
            stats
                .total_incoming_connection_attempts
                .fetch_add(1, Ordering::Relaxed);

            // check overall connection request rate limiter
            if overall_connection_rate_limiter.current_tokens() == 0 {
                stats
                    .connection_rate_limited_across_all
                    .fetch_add(1, Ordering::Relaxed);
                debug!(
                    "Ignoring incoming connection from {} due to overall rate limit.",
                    incoming.remote_address()
                );
                incoming.ignore();
                continue;
            }
            // then perform per IpAddr rate limiting
            if !rate_limiter.is_allowed(&incoming.remote_address().ip()) {
                stats
                    .connection_rate_limited_per_ipaddr
                    .fetch_add(1, Ordering::Relaxed);
                debug!(
                    "Ignoring incoming connection from {} due to per-IP rate limiting.",
                    incoming.remote_address()
                );
                incoming.ignore();
                continue;
            }

            let Ok(client_connection_tracker) =
                ClientConnectionTracker::new(stats.clone(), qos.max_concurrent_connections())
            else {
                stats
                    .refused_connections_too_many_open_connections
                    .fetch_add(1, Ordering::Relaxed);
                incoming.refuse();
                continue;
            };

            stats
                .outstanding_incoming_connection_attempts
                .fetch_add(1, Ordering::Relaxed);
            let connecting = incoming.accept();
            match connecting {
                Ok(connecting) => {
                    let rate_limiter = rate_limiter.clone();
                    let overall_connection_rate_limiter = overall_connection_rate_limiter.clone();
                    tasks.spawn(setup_connection(
                        connecting,
                        rate_limiter,
                        overall_connection_rate_limiter,
                        client_connection_tracker,
                        packet_batch_sender.clone(),
                        stats.clone(),
                        quic_server_params.clone(),
                        qos.clone(),
                        tasks.clone(),
                    ));
                }
                Err(err) => {
                    stats
                        .outstanding_incoming_connection_attempts
                        .fetch_sub(1, Ordering::Relaxed);
                    debug!("Incoming::accept(): error {err:?}");
                }
            }
        } else {
            debug!("accept(): Timed out waiting for connection");
        }
    }
    tasks
}

pub fn get_remote_pubkey(connection: &Connection) -> Option<Pubkey> {
    // Use the client cert only if it is self signed and the chain length is 1.
    connection
        .peer_identity()?
        .downcast::<Vec<rustls::pki_types::CertificateDer>>()
        .ok()
        .filter(|certs| certs.len() == 1)?
        .first()
        .and_then(get_pubkey_from_tls_certificate)
}

pub fn get_connection_stake(
    connection: &Connection,
    staked_nodes: &RwLock<StakedNodes>,
) -> Option<(Pubkey, u64, u64)> {
    let pubkey = get_remote_pubkey(connection)?;
    debug!("Peer public key is {pubkey:?}");
    let staked_nodes = staked_nodes.read().unwrap();
    Some((
        pubkey,
        staked_nodes.get_node_stake(&pubkey)?,
        staked_nodes.total_stake(),
    ))
}

#[derive(Debug)]
pub(crate) enum ConnectionHandlerError {
    ConnectionAddError,
    MaxStreamError,
}

pub(crate) fn update_open_connections_stat<S: OpaqueStreamerCounter>(
    stats: &StreamerStats,
    connection_table: &ConnectionTable<S>,
) {
    if connection_table.is_staked() {
        stats
            .open_staked_connections
            .store(connection_table.table_size(), Ordering::Relaxed);
        stats
            .peak_open_staked_connections
            .fetch_max(connection_table.table_size(), Ordering::Relaxed);
    } else {
        stats
            .open_unstaked_connections
            .store(connection_table.table_size(), Ordering::Relaxed);
        stats
            .peak_open_unstaked_connections
            .fetch_max(connection_table.table_size(), Ordering::Relaxed);
    }
}

#[allow(clippy::too_many_arguments)]
async fn setup_connection<Q, C>(
    connecting: Connecting,
    rate_limiter: Arc<ConnectionRateLimiter>,
    overall_connection_rate_limiter: Arc<TokenBucket>,
    client_connection_tracker: ClientConnectionTracker,
    packet_sender: Sender<PacketBatch>,
    stats: Arc<StreamerStats>,
    server_params: Arc<QuicStreamerConfig>,
    qos: Arc<Q>,
    tasks: TaskTracker,
) where
    Q: QosController<C> + Send + Sync + 'static,
    C: ConnectionContext + Send + Sync + 'static,
{
    let from = connecting.remote_address();
    let res = timeout(QUIC_CONNECTION_HANDSHAKE_TIMEOUT, connecting).await;
    stats
        .outstanding_incoming_connection_attempts
        .fetch_sub(1, Ordering::Relaxed);
    if let Ok(connecting_result) = res {
        match connecting_result {
            Ok(new_connection) => {
                debug!("Got a connection {from:?}");
                // now that we have observed the handshake we can be certain
                // that the initiator owns an IP address, we can update rate
                // limiters on the server
                if !rate_limiter.register_connection(&from.ip()) {
                    debug!("Reject connection from {from:?} -- rate limiting exceeded");
                    stats
                        .connection_rate_limited_per_ipaddr
                        .fetch_add(1, Ordering::Relaxed);
                    new_connection.close(
                        CONNECTION_CLOSE_CODE_DISALLOWED.into(),
                        CONNECTION_CLOSE_REASON_DISALLOWED,
                    );
                    return;
                }

                if overall_connection_rate_limiter.consume_tokens(1).is_err() {
                    debug!(
                        "Reject connection from {:?} -- total rate limiting exceeded",
                        from.ip()
                    );
                    stats
                        .connection_rate_limited_across_all
                        .fetch_add(1, Ordering::Relaxed);
                    new_connection.close(
                        CONNECTION_CLOSE_CODE_DISALLOWED.into(),
                        CONNECTION_CLOSE_REASON_DISALLOWED,
                    );
                    return;
                }

                stats.total_new_connections.fetch_add(1, Ordering::Relaxed);

                let mut conn_context = qos.build_connection_context(&new_connection);
                if let Some(cancel_connection) = qos
                    .try_add_connection(
                        client_connection_tracker,
                        &new_connection,
                        &mut conn_context,
                    )
                    .await
                {
                    tasks.spawn(handle_connection(
                        packet_sender.clone(),
                        new_connection,
                        stats,
                        server_params.wait_for_chunk_timeout,
                        conn_context.clone(),
                        qos,
                        cancel_connection,
                    ));
                }
            }
            Err(e) => {
                handle_connection_error(e, &stats, from);
            }
        }
    } else {
        stats
            .connection_setup_timeout
            .fetch_add(1, Ordering::Relaxed);
    }
}

fn handle_connection_error(e: quinn::ConnectionError, stats: &StreamerStats, from: SocketAddr) {
    debug!("error: {e:?} from: {from:?}");
    stats.connection_setup_error.fetch_add(1, Ordering::Relaxed);
    match e {
        quinn::ConnectionError::TimedOut => {
            stats
                .connection_setup_error_timed_out
                .fetch_add(1, Ordering::Relaxed);
        }
        quinn::ConnectionError::ConnectionClosed(_) => {
            stats
                .connection_setup_error_closed
                .fetch_add(1, Ordering::Relaxed);
        }
        quinn::ConnectionError::TransportError(_) => {
            stats
                .connection_setup_error_transport
                .fetch_add(1, Ordering::Relaxed);
        }
        quinn::ConnectionError::ApplicationClosed(_) => {
            stats
                .connection_setup_error_app_closed
                .fetch_add(1, Ordering::Relaxed);
        }
        quinn::ConnectionError::Reset => {
            stats
                .connection_setup_error_reset
                .fetch_add(1, Ordering::Relaxed);
        }
        quinn::ConnectionError::LocallyClosed => {
            stats
                .connection_setup_error_locally_closed
                .fetch_add(1, Ordering::Relaxed);
        }
        _ => {}
    }
}

fn track_streamer_fetch_packet_performance(
    packet_perf_measure: &[([u8; 64], Instant)],
    stats: &StreamerStats,
) {
    if packet_perf_measure.is_empty() {
        return;
    }
    let mut measure = Measure::start("track_perf");
    let mut process_sampled_packets_us_hist = stats.process_sampled_packets_us_hist.lock().unwrap();

    let now = Instant::now();
    for (signature, start_time) in packet_perf_measure {
        let duration = now.duration_since(*start_time);
        debug!(
            "QUIC streamer fetch stage took {duration:?} for transaction {:?}",
            Signature::from(*signature)
        );
        process_sampled_packets_us_hist
            .increment(duration.as_micros() as u64)
            .unwrap();
    }

    drop(process_sampled_packets_us_hist);
    measure.stop();
    stats
        .perf_track_overhead_us
        .fetch_add(measure.as_us(), Ordering::Relaxed);
}

async fn handle_connection<Q, C>(
    packet_sender: Sender<PacketBatch>,
    connection: Connection,
    stats: Arc<StreamerStats>,
    wait_for_chunk_timeout: Duration,
    context: C,
    qos: Arc<Q>,
    cancel: CancellationToken,
) where
    Q: QosController<C> + Send + Sync + 'static,
    C: ConnectionContext + Send + Sync + 'static,
{
    let peer_type = context.peer_type();
    let remote_addr = connection.remote_address();
    debug!(
        "quic new connection {} streams: {} connections: {}",
        remote_addr,
        stats.active_streams.load(Ordering::Relaxed),
        stats.total_connections.load(Ordering::Relaxed),
    );
    stats.total_connections.fetch_add(1, Ordering::Relaxed);

    'conn: loop {
        // Wait for new streams. If the peer is disconnected we get a cancellation signal and stop
        // the connection task.
        let mut stream = select! {
            stream = connection.accept_uni() => match stream {
                Ok(stream) => stream,
                Err(e) => {
                    debug!("stream error: {e:?}");
                    break;
                }
            },
            _ = cancel.cancelled() => break,
        };

        qos.on_new_stream(&context).await;
        qos.on_stream_accepted(&context);
        stats.active_streams.fetch_add(1, Ordering::Relaxed);
        stats.total_new_streams.fetch_add(1, Ordering::Relaxed);

        let mut meta = Meta::default();
        meta.set_socket_addr(&remote_addr);
        meta.set_from_staked_node(matches!(peer_type, ConnectionPeerType::Staked(_)));
        if let Some(pubkey) = context.remote_pubkey() {
            meta.set_remote_pubkey(pubkey);
        }

        let mut accum = PacketAccumulator::new(meta);
        // Virtually all small transactions will fit in 1 chunk. Larger transactions will fit in 1
        // or 2 chunks if the first chunk starts towards the end of a datagram. A small number of
        // transaction will have other protocol frames inserted in the middle. Empirically it's been
        // observed that 4 is the maximum number of chunks txs get split into.
        //
        // Bytes values are small, so overall the array takes only 128 bytes, and the "cost" of
        // overallocating a few bytes is negligible compared to the cost of having to do multiple
        // read_chunks() calls.
        let mut chunks: [Bytes; 4] = array::from_fn(|_| Bytes::new());

        loop {
            // Read the next chunks, waiting up to `wait_for_chunk_timeout`. If we don't get chunks
            // before then, we assume the stream is dead. This can only happen if there's severe
            // packet loss or the peer stops sending for whatever reason.
            let n_chunks = match tokio::select! {
                chunk = tokio::time::timeout(
                    wait_for_chunk_timeout,
                    stream.read_chunks(&mut chunks)) => chunk,

                // If the peer gets disconnected stop the task right away.
                _ = cancel.cancelled() => break,
            } {
                // read_chunk returned success
                Ok(Ok(chunk)) => chunk.unwrap_or(0),
                // read_chunk returned error
                Ok(Err(e)) => {
                    debug!("Received stream error: {e:?}");
                    stats
                        .total_stream_read_errors
                        .fetch_add(1, Ordering::Relaxed);
                    break;
                }
                // timeout elapsed
                Err(_) => {
                    debug!("Timeout in receiving on stream");
                    stats
                        .total_stream_read_timeouts
                        .fetch_add(1, Ordering::Relaxed);
                    break;
                }
            };

            match handle_chunks(
                // Bytes::clone() is a cheap atomic inc
                chunks.iter().take(n_chunks).cloned(),
                &mut accum,
                &packet_sender,
                &stats,
                peer_type,
            ) {
                // The stream is finished, break out of the loop and close the stream.
                Ok(StreamState::Finished) => {
                    qos.on_stream_finished(&context);
                    break;
                }
                // The stream is still active, continue reading.
                Ok(StreamState::Receiving) => {}
                Err(_) => {
                    // Disconnect peers that send invalid streams.
                    connection.close(
                        CONNECTION_CLOSE_CODE_INVALID_STREAM.into(),
                        CONNECTION_CLOSE_REASON_INVALID_STREAM,
                    );
                    stats.active_streams.fetch_sub(1, Ordering::Relaxed);
                    qos.on_stream_error(&context);
                    break 'conn;
                }
            }
        }

        stats.active_streams.fetch_sub(1, Ordering::Relaxed);
        qos.on_stream_closed(&context);
    }

    let removed_connection_count = qos.remove_connection(&context, connection).await;
    if removed_connection_count > 0 {
        stats
            .connection_removed
            .fetch_add(removed_connection_count, Ordering::Relaxed);
    } else {
        stats
            .connection_remove_failed
            .fetch_add(1, Ordering::Relaxed);
    }
    stats.total_connections.fetch_sub(1, Ordering::Relaxed);
}

enum StreamState {
    // Stream is not finished, keep receiving chunks
    Receiving,
    // Stream is finished
    Finished,
}

// Handle the chunks received from the stream. If the stream is finished, send the packet to the
// packet sender.
//
// Returns Err(()) if the stream is invalid.
fn handle_chunks(
    chunks: impl ExactSizeIterator<Item = Bytes>,
    accum: &mut PacketAccumulator,
    packet_sender: &Sender<PacketBatch>,
    stats: &StreamerStats,
    peer_type: ConnectionPeerType,
) -> Result<StreamState, ()> {
    let n_chunks = chunks.len();
    for chunk in chunks {
        accum.meta.size += chunk.len();
        if accum.meta.size > PACKET_DATA_SIZE {
            // The stream window size is set to PACKET_DATA_SIZE, so one individual chunk can
            // never exceed this size. A peer can send two chunks that together exceed the size
            // tho, in which case we report the error.
            stats.invalid_stream_size.fetch_add(1, Ordering::Relaxed);
            debug!("invalid stream size {}", accum.meta.size);
            return Err(());
        }
        accum.chunks.push(chunk);
        if peer_type.is_staked() {
            stats
                .total_staked_chunks_received
                .fetch_add(1, Ordering::Relaxed);
        } else {
            stats
                .total_unstaked_chunks_received
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    // n_chunks == 0 marks the end of a stream
    if n_chunks != 0 {
        return Ok(StreamState::Receiving);
    }

    if accum.chunks.is_empty() {
        debug!("stream is empty");
        stats
            .total_packet_batches_none
            .fetch_add(1, Ordering::Relaxed);
        return Err(());
    }

    // done receiving chunks
    let bytes_sent = accum.meta.size;

    //
    // 86% of transactions/packets come in one chunk. In that case,
    // we can just move the chunk to the `Packet` and no copy is
    // made.
    // 14% of them come in multiple chunks. In that case, we copy
    // them into one `Bytes` buffer. We make a copy once, with
    // intention to not do it again.
    let mut packet = if accum.chunks.len() == 1 {
        BytesPacket::new(
            accum.chunks.pop().expect("expected one chunk"),
            accum.meta.clone(),
        )
    } else {
        let size: usize = accum.chunks.iter().map(Bytes::len).sum();
        let mut buf = BytesMut::with_capacity(size);
        for chunk in &accum.chunks {
            buf.put_slice(chunk);
        }
        BytesPacket::new(buf.freeze(), accum.meta.clone())
    };

    let packet_size = packet.meta().size;

    let mut packet_perf_measure = None;
    if let Some(signature) = signature_if_should_track_packet(&packet).ok().flatten() {
        packet_perf_measure = Some((*signature, accum.start_time));
        // we set the PERF_TRACK_PACKET on
        packet.meta_mut().set_track_performance(true);
    }
    let packet_batch = PacketBatch::Single(packet);

    if let Err(err) = packet_sender.try_send(packet_batch) {
        stats
            .total_handle_chunk_to_packet_send_err
            .fetch_add(1, Ordering::Relaxed);
        match err {
            TrySendError::Full(_) => {
                stats
                    .total_handle_chunk_to_packet_send_full_err
                    .fetch_add(1, Ordering::Relaxed);
            }
            TrySendError::Disconnected(_) => {
                stats
                    .total_handle_chunk_to_packet_send_disconnected_err
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        trace!("packet batch send error {err:?}");
    } else {
        if let Some(ppm) = &packet_perf_measure {
            track_streamer_fetch_packet_performance(core::array::from_ref(ppm), stats);
        }

        stats
            .total_bytes_sent_to_consumer
            .fetch_add(packet_size, Ordering::Relaxed);
        stats
            .total_packets_sent_to_consumer
            .fetch_add(1, Ordering::Relaxed);

        match peer_type {
            ConnectionPeerType::Unstaked => {
                stats
                    .total_unstaked_packets_sent_for_batching
                    .fetch_add(1, Ordering::Relaxed);
            }
            ConnectionPeerType::Staked(_) => {
                stats
                    .total_staked_packets_sent_for_batching
                    .fetch_add(1, Ordering::Relaxed);
            }
        }

        trace!("sent {bytes_sent} byte packet for batching");
    }

    Ok(StreamState::Finished)
}

struct ConnectionEntry<S: OpaqueStreamerCounter> {
    cancel: CancellationToken,
    peer_type: ConnectionPeerType,
    last_update: Arc<AtomicU64>,
    port: u16,
    // We do not explicitly use it, but its drop is triggered when ConnectionEntry is dropped.
    _client_connection_tracker: ClientConnectionTracker,
    connection: Option<Connection>,
    stream_counter: Arc<S>,
}

impl<S: OpaqueStreamerCounter> ConnectionEntry<S> {
    fn new(
        cancel: CancellationToken,
        peer_type: ConnectionPeerType,
        last_update: Arc<AtomicU64>,
        port: u16,
        client_connection_tracker: ClientConnectionTracker,
        connection: Option<Connection>,
        stream_counter: Arc<S>,
    ) -> Self {
        Self {
            cancel,
            peer_type,
            last_update,
            port,
            _client_connection_tracker: client_connection_tracker,
            connection,
            stream_counter,
        }
    }

    fn last_update(&self) -> u64 {
        self.last_update.load(Ordering::Relaxed)
    }

    fn stake(&self) -> u64 {
        match self.peer_type {
            ConnectionPeerType::Unstaked => 0,
            ConnectionPeerType::Staked(stake) => stake,
        }
    }
}

impl<S: OpaqueStreamerCounter> Drop for ConnectionEntry<S> {
    fn drop(&mut self) {
        if let Some(conn) = self.connection.take() {
            conn.close(
                CONNECTION_CLOSE_CODE_DROPPED_ENTRY.into(),
                CONNECTION_CLOSE_REASON_DROPPED_ENTRY,
            );
        }
        self.cancel.cancel();
    }
}

#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ConnectionTableKey {
    IP(IpAddr),
    Pubkey(Pubkey),
}

impl ConnectionTableKey {
    pub(crate) fn new(ip: IpAddr, maybe_pubkey: Option<Pubkey>) -> Self {
        maybe_pubkey.map_or(ConnectionTableKey::IP(ip), |pubkey| {
            ConnectionTableKey::Pubkey(pubkey)
        })
    }
}

pub(crate) enum ConnectionTableType {
    Staked,
    Unstaked,
}

// Map of IP to list of connection entries
pub(crate) struct ConnectionTable<S: OpaqueStreamerCounter> {
    table: IndexMap<ConnectionTableKey, Vec<ConnectionEntry<S>>>,
    pub(crate) total_size: usize,
    table_type: ConnectionTableType,
    cancel: CancellationToken,
}

/// Prune the connection which has the oldest update
///
/// Return number pruned
impl<S: OpaqueStreamerCounter> ConnectionTable<S> {
    pub(crate) fn new(table_type: ConnectionTableType, cancel: CancellationToken) -> Self {
        Self {
            table: IndexMap::default(),
            total_size: 0,
            table_type,
            cancel,
        }
    }

    fn table_size(&self) -> usize {
        self.total_size
    }

    fn is_staked(&self) -> bool {
        matches!(self.table_type, ConnectionTableType::Staked)
    }

    pub(crate) fn prune_oldest(&mut self, max_size: usize) -> usize {
        let mut num_pruned = 0;
        let key = |(_, connections): &(_, &Vec<_>)| {
            connections.iter().map(ConnectionEntry::last_update).min()
        };
        while self.total_size.saturating_sub(num_pruned) > max_size {
            match self.table.values().enumerate().min_by_key(key) {
                None => break,
                Some((index, connections)) => {
                    num_pruned += connections.len();
                    self.table.swap_remove_index(index);
                }
            }
        }
        self.total_size = self.total_size.saturating_sub(num_pruned);
        num_pruned
    }

    // Randomly selects sample_size many connections, evicts the one with the
    // lowest stake, and returns the number of pruned connections.
    // If the stakes of all the sampled connections are higher than the
    // threshold_stake, rejects the pruning attempt, and returns 0.
    pub(crate) fn prune_random(&mut self, sample_size: usize, threshold_stake: u64) -> usize {
        let num_pruned = std::iter::once(self.table.len())
            .filter(|&size| size > 0)
            .flat_map(|size| {
                let mut rng = rng();
                repeat_with(move || rng.random_range(0..size))
            })
            .map(|index| {
                let connection = self.table[index].first();
                let stake = connection.map(|connection: &ConnectionEntry<S>| connection.stake());
                (index, stake)
            })
            .take(sample_size)
            .min_by_key(|&(_, stake)| stake)
            .filter(|&(_, stake)| stake < Some(threshold_stake))
            .and_then(|(index, _)| self.table.swap_remove_index(index))
            .map(|(_, connections)| connections.len())
            .unwrap_or_default();
        self.total_size = self.total_size.saturating_sub(num_pruned);
        num_pruned
    }

    pub(crate) fn try_add_connection<F: FnOnce() -> Arc<S>>(
        &mut self,
        key: ConnectionTableKey,
        port: u16,
        client_connection_tracker: ClientConnectionTracker,
        connection: Option<Connection>,
        peer_type: ConnectionPeerType,
        last_update: Arc<AtomicU64>,
        max_connections_per_peer: usize,
        stream_counter_factory: F,
    ) -> Option<(Arc<AtomicU64>, CancellationToken, Arc<S>)> {
        let connection_entry = self.table.entry(key).or_default();
        let has_connection_capacity = connection_entry
            .len()
            .checked_add(1)
            .map(|c| c <= max_connections_per_peer)
            .unwrap_or(false);
        if has_connection_capacity {
            let cancel = self.cancel.child_token();
            let stream_counter = connection_entry
                .first()
                .map(|entry| entry.stream_counter.clone())
                .unwrap_or_else(stream_counter_factory);
            connection_entry.push(ConnectionEntry::new(
                cancel.clone(),
                peer_type,
                last_update.clone(),
                port,
                client_connection_tracker,
                connection,
                stream_counter.clone(),
            ));
            self.total_size += 1;
            Some((last_update, cancel, stream_counter))
        } else {
            if let Some(connection) = connection {
                connection.close(
                    CONNECTION_CLOSE_CODE_TOO_MANY.into(),
                    CONNECTION_CLOSE_REASON_TOO_MANY,
                );
            }
            None
        }
    }

    // Returns number of connections that were removed
    pub(crate) fn remove_connection(
        &mut self,
        key: ConnectionTableKey,
        port: u16,
        stable_id: usize,
    ) -> usize {
        if let Entry::Occupied(mut e) = self.table.entry(key) {
            let e_ref = e.get_mut();
            let old_size = e_ref.len();

            e_ref.retain(|connection_entry| {
                // Retain the connection entry if the port is different, or if the connection's
                // stable_id doesn't match the provided stable_id.
                // (Some unit tests do not fill in a valid connection in the table. To support that,
                // if the connection is none, the stable_id check is ignored. i.e. if the port matches,
                // the connection gets removed)
                connection_entry.port != port
                    || connection_entry
                        .connection
                        .as_ref()
                        .and_then(|connection| (connection.stable_id() != stable_id).then_some(0))
                        .is_some()
            });
            let new_size = e_ref.len();
            if e_ref.is_empty() {
                e.swap_remove_entry();
            }
            let connections_removed = old_size.saturating_sub(new_size);
            self.total_size = self.total_size.saturating_sub(connections_removed);
            connections_removed
        } else {
            0
        }
    }
}

struct EndpointAccept<'a> {
    endpoint: usize,
    accept: Accept<'a>,
}

impl Future for EndpointAccept<'_> {
    type Output = (Option<quinn::Incoming>, usize);

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context) -> Poll<Self::Output> {
        let i = self.endpoint;
        // Safety:
        // self is pinned and accept is a field so it can't get moved out. See safety docs of
        // map_unchecked_mut.
        unsafe { self.map_unchecked_mut(|this| &mut this.accept) }
            .poll(cx)
            .map(|r| (r, i))
    }
}

#[cfg(test)]
pub mod test {
    use {
        super::*,
        crate::nonblocking::{
            qos::NullStreamerCounter,
            swqos::SwQosConfig,
            testing_utilities::{
                check_multiple_streams, get_client_config, make_client_endpoint, setup_quic_server,
                spawn_stake_weighted_qos_server, SpawnTestServerResult,
            },
        },
        assert_matches::assert_matches,
        crossbeam_channel::{unbounded, Receiver},
        quinn::{ApplicationClose, ConnectionError},
        solana_keypair::Keypair,
        solana_net_utils::sockets::bind_to_localhost_unique,
        solana_signer::Signer,
        std::collections::HashMap,
        tokio::time::sleep,
    };

    pub async fn check_timeout(receiver: Receiver<PacketBatch>, server_address: SocketAddr) {
        let conn1 = make_client_endpoint(&server_address, None).await;
        let total = 30;
        for i in 0..total {
            let mut s1 = conn1.open_uni().await.unwrap();
            s1.write_all(&[0u8]).await.unwrap();
            s1.finish().unwrap();
            info!("done {i}");
            sleep(Duration::from_millis(1000)).await;
        }
        let mut received = 0;
        loop {
            if let Ok(_x) = receiver.try_recv() {
                received += 1;
                info!("got {received}");
            } else {
                sleep(Duration::from_millis(500)).await;
            }
            if received >= total {
                break;
            }
        }
    }

    pub async fn check_block_multiple_connections(server_address: SocketAddr) {
        let conn1 = make_client_endpoint(&server_address, None).await;
        let conn2 = make_client_endpoint(&server_address, None).await;
        let mut s1 = conn1.open_uni().await.unwrap();
        let s2 = conn2.open_uni().await;
        if let Ok(mut s2) = s2 {
            s1.write_all(&[0u8]).await.unwrap();
            s1.finish().unwrap();
            // Send enough data to create more than 1 chunks.
            // The first will try to open the connection (which should fail).
            // The following chunks will enable the detection of connection failure.
            let data = vec![1u8; PACKET_DATA_SIZE * 2];
            s2.write_all(&data)
                .await
                .expect_err("shouldn't be able to open 2 connections");
        } else {
            // It has been noticed if there is already connection open against the server, this open_uni can fail
            // with ApplicationClosed(ApplicationClose) error due to CONNECTION_CLOSE_CODE_TOO_MANY before writing to
            // the stream -- expect it.
            assert_matches!(s2, Err(quinn::ConnectionError::ApplicationClosed(_)));
        }
    }

    pub async fn check_multiple_writes(
        receiver: Receiver<PacketBatch>,
        server_address: SocketAddr,
        client_keypair: Option<&Keypair>,
    ) {
        let conn1 = Arc::new(make_client_endpoint(&server_address, client_keypair).await);

        // Send a full size packet with single byte writes.
        let num_bytes = PACKET_DATA_SIZE;
        let num_expected_packets = 1;
        let mut s1 = conn1.open_uni().await.unwrap();
        for _ in 0..num_bytes {
            s1.write_all(&[0u8]).await.unwrap();
        }
        s1.finish().unwrap();

        check_received_packets(receiver, num_expected_packets, num_bytes).await;
    }

    pub async fn check_multiple_packets(
        receiver: Receiver<PacketBatch>,
        server_address: SocketAddr,
        client_keypair: Option<&Keypair>,
        num_expected_packets: usize,
    ) {
        let conn1 = Arc::new(make_client_endpoint(&server_address, client_keypair).await);

        // Send a full size packet with single byte writes.
        let num_bytes = PACKET_DATA_SIZE;
        let packet = vec![1u8; num_bytes];
        for _ in 0..num_expected_packets {
            let mut s1 = conn1.open_uni().await.unwrap();
            s1.write_all(&packet).await.unwrap();
            s1.finish().unwrap();
        }

        check_received_packets(receiver, num_expected_packets, num_bytes).await;
    }

    async fn check_received_packets(
        receiver: Receiver<PacketBatch>,
        num_expected_packets: usize,
        num_bytes: usize,
    ) {
        let mut all_packets = vec![];
        let now = Instant::now();
        let mut total_packets = 0;
        while now.elapsed().as_secs() < 5 {
            // We're running in an async environment, we (almost) never
            // want to block
            if let Ok(packets) = receiver.try_recv() {
                total_packets += packets.len();
                all_packets.push(packets)
            } else {
                sleep(Duration::from_secs(1)).await;
            }
            if total_packets >= num_expected_packets {
                break;
            }
        }
        for batch in all_packets {
            for p in batch.iter() {
                assert_eq!(p.meta().size, num_bytes);
            }
        }
        assert_eq!(total_packets, num_expected_packets);
    }

    pub async fn check_unstaked_node_connect_failure(server_address: SocketAddr) {
        let conn1 = Arc::new(make_client_endpoint(&server_address, None).await);

        // Send a full size packet with single byte writes.
        if let Ok(mut s1) = conn1.open_uni().await {
            for _ in 0..PACKET_DATA_SIZE {
                // Ignoring any errors here. s1.finish() will test the error condition
                s1.write_all(&[0u8]).await.unwrap_or_default();
            }
            s1.finish().unwrap_or_default();
            s1.stopped().await.unwrap_err();
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_quic_server_exit_on_cancel() {
        let SpawnTestServerResult {
            join_handle,
            receiver,
            server_address: _,
            stats: _,
            cancel,
        } = setup_quic_server(
            None,
            QuicStreamerConfig::default_for_tests(),
            SwQosConfig::default(),
        );
        cancel.cancel();
        join_handle.await.unwrap();
        // test that it is stopped by cancel, not due to receiver
        // dropped.
        drop(receiver);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_quic_timeout() {
        agave_logger::setup();
        let SpawnTestServerResult {
            join_handle,
            receiver,
            server_address,
            stats: _,
            cancel,
        } = setup_quic_server(
            None,
            QuicStreamerConfig::default_for_tests(),
            SwQosConfig::default(),
        );

        check_timeout(receiver, server_address).await;
        cancel.cancel();
        join_handle.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_quic_stream_timeout() {
        agave_logger::setup();
        let SpawnTestServerResult {
            join_handle,
            receiver,
            server_address,
            stats,
            cancel,
        } = setup_quic_server(
            None,
            QuicStreamerConfig::default_for_tests(),
            SwQosConfig::default(),
        );

        let conn1 = make_client_endpoint(&server_address, None).await;
        assert_eq!(stats.active_streams.load(Ordering::Relaxed), 0);
        assert_eq!(stats.total_stream_read_timeouts.load(Ordering::Relaxed), 0);

        // Send one byte to start the stream
        let mut s1 = conn1.open_uni().await.unwrap();
        s1.write_all(&[0u8]).await.unwrap_or_default();

        // Wait long enough for the stream to timeout in receiving chunks
        let sleep_time = DEFAULT_WAIT_FOR_CHUNK_TIMEOUT * 2;
        sleep(sleep_time).await;

        // Test that the stream was created, but timed out in read
        assert_eq!(stats.active_streams.load(Ordering::Relaxed), 0);
        assert_ne!(stats.total_stream_read_timeouts.load(Ordering::Relaxed), 0);

        // Test that more writes to the stream will fail (i.e. the stream is no longer writable
        // after the timeouts)
        assert!(s1.write_all(&[0u8]).await.is_err());

        cancel.cancel();
        drop(receiver);
        join_handle.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_quic_server_block_multiple_connections() {
        agave_logger::setup();
        let SpawnTestServerResult {
            join_handle,
            receiver,
            server_address,
            stats: _,
            cancel,
        } = setup_quic_server(
            None,
            QuicStreamerConfig::default_for_tests(),
            SwQosConfig::default_for_tests(),
        );
        check_block_multiple_connections(server_address).await;
        cancel.cancel();
        drop(receiver);
        join_handle.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_quic_server_multiple_connections_on_single_client_endpoint() {
        agave_logger::setup();

        let SpawnTestServerResult {
            join_handle,
            receiver,
            server_address,
            stats,
            cancel,
        } = setup_quic_server(
            None,
            QuicStreamerConfig {
                ..QuicStreamerConfig::default_for_tests()
            },
            SwQosConfig {
                max_connections_per_unstaked_peer: 2,
                ..SwQosConfig::default_for_tests()
            },
        );

        let client_socket = bind_to_localhost_unique().expect("should bind - client");
        let mut endpoint = quinn::Endpoint::new(
            EndpointConfig::default(),
            None,
            client_socket,
            Arc::new(TokioRuntime),
        )
        .unwrap();
        let default_keypair = Keypair::new();
        endpoint.set_default_client_config(get_client_config(&default_keypair));
        let conn1 = endpoint
            .connect(server_address, "localhost")
            .expect("Failed in connecting")
            .await
            .expect("Failed in waiting");

        let conn2 = endpoint
            .connect(server_address, "localhost")
            .expect("Failed in connecting")
            .await
            .expect("Failed in waiting");

        let mut s1 = conn1.open_uni().await.unwrap();
        s1.write_all(&[0u8]).await.unwrap();
        s1.finish().unwrap();

        let mut s2 = conn2.open_uni().await.unwrap();
        conn1.close(
            CONNECTION_CLOSE_CODE_DROPPED_ENTRY.into(),
            CONNECTION_CLOSE_REASON_DROPPED_ENTRY,
        );

        let start = Instant::now();
        while stats.connection_removed.load(Ordering::Relaxed) != 1 && start.elapsed().as_secs() < 1
        {
            debug!("First connection not removed yet");
            sleep(Duration::from_millis(10)).await;
        }
        assert!(start.elapsed().as_secs() < 1);

        s2.write_all(&[0u8]).await.unwrap();
        s2.finish().unwrap();

        conn2.close(
            CONNECTION_CLOSE_CODE_DROPPED_ENTRY.into(),
            CONNECTION_CLOSE_REASON_DROPPED_ENTRY,
        );

        let start = Instant::now();
        while stats.connection_removed.load(Ordering::Relaxed) != 2 && start.elapsed().as_secs() < 1
        {
            debug!("Second connection not removed yet");
            sleep(Duration::from_millis(10)).await;
        }
        assert!(start.elapsed().as_secs() < 1);

        cancel.cancel();
        // Explicitly drop receiver here so that it doesn't get implicitly
        // dropped earlier. This is necessary to ensure the server stays alive
        // and doesn't issue a cancel to kill the connection earlier than
        // expected.
        drop(receiver);
        join_handle.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_quic_server_multiple_writes() {
        agave_logger::setup();
        let SpawnTestServerResult {
            join_handle,
            receiver,
            server_address,
            stats: _,
            cancel,
        } = setup_quic_server(
            None,
            QuicStreamerConfig::default_for_tests(),
            SwQosConfig::default(),
        );
        check_multiple_writes(receiver, server_address, None).await;
        cancel.cancel();
        join_handle.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_quic_server_staked_connection_removal() {
        agave_logger::setup();

        let client_keypair = Keypair::new();
        let stakes = HashMap::from([(client_keypair.pubkey(), 100_000)]);
        let staked_nodes = StakedNodes::new(
            Arc::new(stakes),
            HashMap::<Pubkey, u64>::default(), // overrides
        );
        let SpawnTestServerResult {
            join_handle,
            receiver,
            server_address,
            stats,
            cancel,
        } = setup_quic_server(
            Some(staked_nodes),
            QuicStreamerConfig::default_for_tests(),
            SwQosConfig::default(),
        );
        check_multiple_writes(receiver, server_address, Some(&client_keypair)).await;
        cancel.cancel();
        join_handle.await.unwrap();

        assert_eq!(
            stats
                .connection_added_from_staked_peer
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(stats.connection_removed.load(Ordering::Relaxed), 1);
        assert_eq!(stats.connection_remove_failed.load(Ordering::Relaxed), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_quic_server_zero_staked_connection_removal() {
        // In this test, the client has a pubkey, but is not in stake table.
        agave_logger::setup();

        let client_keypair = Keypair::new();
        let stakes = HashMap::from([(client_keypair.pubkey(), 0)]);
        let staked_nodes = StakedNodes::new(
            Arc::new(stakes),
            HashMap::<Pubkey, u64>::default(), // overrides
        );
        let SpawnTestServerResult {
            join_handle,
            receiver,
            server_address,
            stats,
            cancel,
        } = setup_quic_server(
            Some(staked_nodes),
            QuicStreamerConfig::default_for_tests(),
            SwQosConfig::default(),
        );
        check_multiple_writes(receiver, server_address, Some(&client_keypair)).await;
        cancel.cancel();
        join_handle.await.unwrap();

        assert_eq!(
            stats
                .connection_added_from_staked_peer
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(stats.connection_removed.load(Ordering::Relaxed), 1);
        assert_eq!(stats.connection_remove_failed.load(Ordering::Relaxed), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_quic_server_unstaked_connection_removal() {
        agave_logger::setup();
        let SpawnTestServerResult {
            join_handle,
            receiver,
            server_address,
            stats,
            cancel,
        } = setup_quic_server(
            None,
            QuicStreamerConfig::default_for_tests(),
            SwQosConfig::default(),
        );
        check_multiple_writes(receiver, server_address, None).await;
        cancel.cancel();
        join_handle.await.unwrap();

        assert_eq!(
            stats
                .connection_added_from_staked_peer
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(stats.connection_removed.load(Ordering::Relaxed), 1);
        assert_eq!(stats.connection_remove_failed.load(Ordering::Relaxed), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_quic_server_unstaked_node_connect_failure() {
        agave_logger::setup();
        let s = bind_to_localhost_unique().expect("should bind");
        let (sender, _) = unbounded();
        let keypair = Keypair::new();
        let server_address = s.local_addr().unwrap();
        let staked_nodes = Arc::new(RwLock::new(StakedNodes::default()));
        let cancel = CancellationToken::new();
        let SpawnNonBlockingServerResult {
            endpoints: _,
            stats: _,
            thread: t,
            max_concurrent_connections: _,
        } = spawn_stake_weighted_qos_server(
            "quic_streamer_test",
            [s],
            &keypair,
            sender,
            staked_nodes,
            QuicStreamerConfig {
                ..QuicStreamerConfig::default_for_tests()
            },
            SwQosConfig {
                max_unstaked_connections: 0, // Do not allow any connection from unstaked clients/nodes
                ..Default::default()
            },
            cancel.clone(),
        )
        .unwrap();

        check_unstaked_node_connect_failure(server_address).await;
        cancel.cancel();
        t.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_quic_server_multiple_streams() {
        agave_logger::setup();
        let s = bind_to_localhost_unique().expect("should bind");
        let (sender, receiver) = unbounded();
        let keypair = Keypair::new();
        let server_address = s.local_addr().unwrap();
        let staked_nodes = Arc::new(RwLock::new(StakedNodes::default()));
        let cancel = CancellationToken::new();
        let SpawnNonBlockingServerResult {
            endpoints: _,
            stats,
            thread: t,
            max_concurrent_connections: _,
        } = spawn_stake_weighted_qos_server(
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

        check_multiple_streams(receiver, server_address, None).await;
        assert_eq!(stats.active_streams.load(Ordering::Relaxed), 0);
        assert_eq!(stats.total_new_streams.load(Ordering::Relaxed), 20);
        assert_eq!(stats.total_connections.load(Ordering::Relaxed), 2);
        assert_eq!(stats.total_new_connections.load(Ordering::Relaxed), 2);
        cancel.cancel();
        t.await.unwrap();

        assert_eq!(stats.total_connections.load(Ordering::Relaxed), 0);
        assert_eq!(stats.total_new_connections.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn test_prune_table_with_ip() {
        use std::net::Ipv4Addr;
        agave_logger::setup();
        let cancel = CancellationToken::new();
        let mut table = ConnectionTable::new(ConnectionTableType::Unstaked, cancel);
        let mut num_entries = 5;
        let max_connections_per_peer = 10;
        let sockets: Vec<_> = (0..num_entries)
            .map(|i| SocketAddr::new(IpAddr::V4(Ipv4Addr::new(i, 0, 0, 0)), 0))
            .collect();
        let stats = Arc::new(StreamerStats::default());
        for (i, socket) in sockets.iter().enumerate() {
            table
                .try_add_connection(
                    ConnectionTableKey::IP(socket.ip()),
                    socket.port(),
                    ClientConnectionTracker::new(stats.clone(), 1000).unwrap(),
                    None,
                    ConnectionPeerType::Unstaked,
                    Arc::new(AtomicU64::new(i as u64)),
                    max_connections_per_peer,
                    || Arc::new(NullStreamerCounter {}),
                )
                .unwrap();
        }
        num_entries += 1;
        table
            .try_add_connection(
                ConnectionTableKey::IP(sockets[0].ip()),
                sockets[0].port(),
                ClientConnectionTracker::new(stats.clone(), 1000).unwrap(),
                None,
                ConnectionPeerType::Unstaked,
                Arc::new(AtomicU64::new(5)),
                max_connections_per_peer,
                || Arc::new(NullStreamerCounter {}),
            )
            .unwrap();

        let new_size = 3;
        let pruned = table.prune_oldest(new_size);
        assert_eq!(pruned, num_entries as usize - new_size);
        for v in table.table.values() {
            for x in v {
                assert!((x.last_update() + 1) >= (num_entries as u64 - new_size as u64));
            }
        }
        assert_eq!(table.table.len(), new_size);
        assert_eq!(table.total_size, new_size);
        for socket in sockets.iter().take(num_entries as usize).skip(new_size - 1) {
            table.remove_connection(ConnectionTableKey::IP(socket.ip()), socket.port(), 0);
        }
        assert_eq!(table.total_size, 0);
        assert_eq!(stats.open_connections.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_prune_table_with_unique_pubkeys() {
        agave_logger::setup();
        let cancel = CancellationToken::new();
        let mut table = ConnectionTable::new(ConnectionTableType::Unstaked, cancel);

        // We should be able to add more entries than max_connections_per_peer, since each entry is
        // from a different peer pubkey.
        let num_entries = 15;
        let max_connections_per_peer = 10;
        let stats = Arc::new(StreamerStats::default());

        let pubkeys: Vec<_> = (0..num_entries).map(|_| Pubkey::new_unique()).collect();
        for (i, pubkey) in pubkeys.iter().enumerate() {
            table
                .try_add_connection(
                    ConnectionTableKey::Pubkey(*pubkey),
                    0,
                    ClientConnectionTracker::new(stats.clone(), 1000).unwrap(),
                    None,
                    ConnectionPeerType::Unstaked,
                    Arc::new(AtomicU64::new(i as u64)),
                    max_connections_per_peer,
                    || Arc::new(NullStreamerCounter {}),
                )
                .unwrap();
        }

        let new_size = 3;
        let pruned = table.prune_oldest(new_size);
        assert_eq!(pruned, num_entries as usize - new_size);
        assert_eq!(table.table.len(), new_size);
        assert_eq!(table.total_size, new_size);
        for pubkey in pubkeys.iter().take(num_entries as usize).skip(new_size - 1) {
            table.remove_connection(ConnectionTableKey::Pubkey(*pubkey), 0, 0);
        }
        assert_eq!(table.total_size, 0);
        assert_eq!(stats.open_connections.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_prune_table_with_non_unique_pubkeys() {
        agave_logger::setup();
        let cancel = CancellationToken::new();
        let mut table = ConnectionTable::new(ConnectionTableType::Unstaked, cancel);

        let max_connections_per_peer = 10;
        let pubkey = Pubkey::new_unique();
        let stats: Arc<StreamerStats> = Arc::new(StreamerStats::default());

        (0..max_connections_per_peer).for_each(|i| {
            table
                .try_add_connection(
                    ConnectionTableKey::Pubkey(pubkey),
                    0,
                    ClientConnectionTracker::new(stats.clone(), 1000).unwrap(),
                    None,
                    ConnectionPeerType::Unstaked,
                    Arc::new(AtomicU64::new(i as u64)),
                    max_connections_per_peer,
                    || Arc::new(NullStreamerCounter {}),
                )
                .unwrap();
        });

        // We should NOT be able to add more entries than max_connections_per_peer, since we are
        // using the same peer pubkey.
        assert!(table
            .try_add_connection(
                ConnectionTableKey::Pubkey(pubkey),
                0,
                ClientConnectionTracker::new(stats.clone(), 1000).unwrap(),
                None,
                ConnectionPeerType::Unstaked,
                Arc::new(AtomicU64::new(10)),
                max_connections_per_peer,
                || Arc::new(NullStreamerCounter {})
            )
            .is_none());

        // We should be able to add an entry from another peer pubkey
        let num_entries = max_connections_per_peer + 1;
        let pubkey2 = Pubkey::new_unique();
        assert!(table
            .try_add_connection(
                ConnectionTableKey::Pubkey(pubkey2),
                0,
                ClientConnectionTracker::new(stats.clone(), 1000).unwrap(),
                None,
                ConnectionPeerType::Unstaked,
                Arc::new(AtomicU64::new(10)),
                max_connections_per_peer,
                || Arc::new(NullStreamerCounter {})
            )
            .is_some());

        assert_eq!(table.total_size, num_entries);

        let new_max_size = 3;
        let pruned = table.prune_oldest(new_max_size);
        assert!(pruned >= num_entries - new_max_size);
        assert!(table.table.len() <= new_max_size);
        assert!(table.total_size <= new_max_size);

        table.remove_connection(ConnectionTableKey::Pubkey(pubkey2), 0, 0);
        assert_eq!(table.total_size, 0);
        assert_eq!(stats.open_connections.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_prune_table_random() {
        use std::net::Ipv4Addr;
        agave_logger::setup();
        let cancel = CancellationToken::new();
        let mut table = ConnectionTable::new(ConnectionTableType::Unstaked, cancel);

        let num_entries = 5;
        let max_connections_per_peer = 10;
        let sockets: Vec<_> = (0..num_entries)
            .map(|i| SocketAddr::new(IpAddr::V4(Ipv4Addr::new(i, 0, 0, 0)), 0))
            .collect();
        let stats: Arc<StreamerStats> = Arc::new(StreamerStats::default());

        for (i, socket) in sockets.iter().enumerate() {
            table
                .try_add_connection(
                    ConnectionTableKey::IP(socket.ip()),
                    socket.port(),
                    ClientConnectionTracker::new(stats.clone(), 1000).unwrap(),
                    None,
                    ConnectionPeerType::Staked((i + 1) as u64),
                    Arc::new(AtomicU64::new(i as u64)),
                    max_connections_per_peer,
                    || Arc::new(NullStreamerCounter {}),
                )
                .unwrap();
        }

        // Try pruninng with threshold stake less than all the entries in the table
        // It should fail to prune (i.e. return 0 number of pruned entries)
        let pruned = table.prune_random(/*sample_size:*/ 2, /*threshold_stake:*/ 0);
        assert_eq!(pruned, 0);

        // Try pruninng with threshold stake higher than all the entries in the table
        // It should succeed to prune (i.e. return 1 number of pruned entries)
        let pruned = table.prune_random(
            2,                      // sample_size
            num_entries as u64 + 1, // threshold_stake
        );
        assert_eq!(pruned, 1);
        // We had 5 connections and pruned 1, we should have 4 left
        assert_eq!(stats.open_connections.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn test_remove_connections() {
        use std::net::Ipv4Addr;
        agave_logger::setup();
        let cancel = CancellationToken::new();
        let mut table = ConnectionTable::new(ConnectionTableType::Unstaked, cancel);

        let num_ips = 5;
        let max_connections_per_peer = 10;
        let mut sockets: Vec<_> = (0..num_ips)
            .map(|i| SocketAddr::new(IpAddr::V4(Ipv4Addr::new(i, 0, 0, 0)), 0))
            .collect();
        let stats: Arc<StreamerStats> = Arc::new(StreamerStats::default());

        for (i, socket) in sockets.iter().enumerate() {
            table
                .try_add_connection(
                    ConnectionTableKey::IP(socket.ip()),
                    socket.port(),
                    ClientConnectionTracker::new(stats.clone(), 1000).unwrap(),
                    None,
                    ConnectionPeerType::Unstaked,
                    Arc::new(AtomicU64::new((i * 2) as u64)),
                    max_connections_per_peer,
                    || Arc::new(NullStreamerCounter {}),
                )
                .unwrap();

            table
                .try_add_connection(
                    ConnectionTableKey::IP(socket.ip()),
                    socket.port(),
                    ClientConnectionTracker::new(stats.clone(), 1000).unwrap(),
                    None,
                    ConnectionPeerType::Unstaked,
                    Arc::new(AtomicU64::new((i * 2 + 1) as u64)),
                    max_connections_per_peer,
                    || Arc::new(NullStreamerCounter {}),
                )
                .unwrap();
        }

        let single_connection_addr =
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(num_ips, 0, 0, 0)), 0);
        table
            .try_add_connection(
                ConnectionTableKey::IP(single_connection_addr.ip()),
                single_connection_addr.port(),
                ClientConnectionTracker::new(stats.clone(), 1000).unwrap(),
                None,
                ConnectionPeerType::Unstaked,
                Arc::new(AtomicU64::new((num_ips * 2) as u64)),
                max_connections_per_peer,
                || Arc::new(NullStreamerCounter {}),
            )
            .unwrap();

        let zero_connection_addr =
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(num_ips + 1, 0, 0, 0)), 0);

        sockets.push(single_connection_addr);
        sockets.push(zero_connection_addr);

        for socket in sockets.iter() {
            table.remove_connection(ConnectionTableKey::IP(socket.ip()), socket.port(), 0);
        }
        assert_eq!(table.total_size, 0);
        assert_eq!(stats.open_connections.load(Ordering::Relaxed), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_throttling_check_no_packet_drop() {
        agave_logger::setup_with_default_filter();

        let SpawnTestServerResult {
            join_handle,
            receiver,
            server_address,
            stats,
            cancel,
        } = setup_quic_server(
            None,
            QuicStreamerConfig::default_for_tests(),
            SwQosConfig::default(),
        );

        let client_connection = make_client_endpoint(&server_address, None).await;

        // unstaked connection can handle up to 100tps, so we should send in ~1s.
        let expected_num_txs = 100;
        let start_time = tokio::time::Instant::now();
        for i in 0..expected_num_txs {
            let mut send_stream = client_connection.open_uni().await.unwrap();
            let data = format!("{i}").into_bytes();
            send_stream.write_all(&data).await.unwrap();
            send_stream.finish().unwrap();
        }
        let elapsed_sending: f64 = start_time.elapsed().as_secs_f64();
        info!("Elapsed sending: {elapsed_sending}");

        // check that delivered all of them
        let start_time = tokio::time::Instant::now();
        let mut num_txs_received = 0;
        while num_txs_received < expected_num_txs && start_time.elapsed() < Duration::from_secs(2) {
            if let Ok(packets) = receiver.try_recv() {
                num_txs_received += packets.len();
            } else {
                sleep(Duration::from_millis(100)).await;
            }
        }
        assert_eq!(expected_num_txs, num_txs_received);

        cancel.cancel();
        join_handle.await.unwrap();

        assert_eq!(
            stats.total_new_streams.load(Ordering::Relaxed),
            expected_num_txs
        );
        assert!(stats.throttled_unstaked_streams.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn test_client_connection_tracker() {
        let stats = Arc::new(StreamerStats::default());
        let tracker_1 = ClientConnectionTracker::new(stats.clone(), 1);
        assert!(tracker_1.is_ok());
        assert!(ClientConnectionTracker::new(stats.clone(), 1).is_err());
        assert_eq!(stats.open_connections.load(Ordering::Relaxed), 1);
        // dropping the connection, concurrent connections should become 0
        drop(tracker_1);
        assert_eq!(stats.open_connections.load(Ordering::Relaxed), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_client_connection_close_invalid_stream() {
        let SpawnTestServerResult {
            join_handle,
            server_address,
            stats,
            cancel,
            ..
        } = setup_quic_server(
            None,
            QuicStreamerConfig::default_for_tests(),
            SwQosConfig::default(),
        );

        let client_connection = make_client_endpoint(&server_address, None).await;

        let mut send_stream = client_connection.open_uni().await.unwrap();
        send_stream
            .write_all(&[42; PACKET_DATA_SIZE + 1])
            .await
            .unwrap();
        match client_connection.closed().await {
            ConnectionError::ApplicationClosed(ApplicationClose { error_code, reason }) => {
                assert_eq!(error_code, CONNECTION_CLOSE_CODE_INVALID_STREAM.into());
                assert_eq!(reason, CONNECTION_CLOSE_REASON_INVALID_STREAM);
            }
            _ => panic!("unexpected close"),
        }
        assert_eq!(stats.invalid_stream_size.load(Ordering::Relaxed), 1);
        cancel.cancel();
        join_handle.await.unwrap();
    }
}
