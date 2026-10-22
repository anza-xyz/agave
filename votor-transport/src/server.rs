//! Inbound (server) direction: we-accept, receive-only.
use {
    crate::{
        HANDSHAKE_TIMEOUT, MAX_INBOUND_CONNECTIONS_PER_PEER, METRICS_INTERVAL,
        PEER_RATE_LIMIT_BURST_WINDOW, PEER_RATE_LIMIT_DOS_WINDOW, PeerListReceiver, close_codes,
        endpoint::{BanCommand, Datagram, KeyUpdateListener},
        error::Error,
        stats::{self, ServerStats, record_server_error},
        transport::new_server_config,
    },
    arrayvec::ArrayVec,
    crossbeam_channel::{Sender, TrySendError},
    log::{debug, error, info, warn},
    quinn::{Connecting, Connection, Endpoint},
    solana_keypair::Signer,
    solana_net_utils::{SocketAddrSpace, banlist::Banlist, token_bucket::TokenBucket},
    solana_pubkey::{Pubkey, PubkeyHasherBuilder},
    solana_tls_utils::get_remote_pubkey,
    std::{
        collections::{HashMap, hash_map::Entry},
        net::{IpAddr, SocketAddr},
        sync::{Arc, atomic::Ordering},
        time::Duration,
    },
    tokio::{
        sync::mpsc,
        task::JoinSet,
        time::{Instant, MissedTickBehavior, interval, sleep, timeout},
    },
    tokio_util::sync::CancellationToken,
};

/// Tracks resource use by one peer
pub(crate) struct PeerEntry {
    connections: ArrayVec<Connection, MAX_INBOUND_CONNECTIONS_PER_PEER>,
    /// Shared ingress data ratelimiter for all connections of this peer.
    rate_limiter: Arc<TokenBucket>,
}

/// Event reported to the InboundLoop.
pub(crate) enum InboundConnectionEvent {
    /// A TLS handshake completed and yielded a valid, authenticated peer.
    Accepted {
        peer: Pubkey,
        connection: Connection,
    },
    /// An inbound connection terminated. `stable_id` identifies the connection.
    Closed { peer: Pubkey, stable_id: usize },
    /// The ingress traffic shaping bucket was drained by a sustained flood.
    FloodDetected { peer: Pubkey },
}

fn is_invalid_remote_address(
    remote_addr: SocketAddr,
    local_ip: Option<IpAddr>,
    socket_addr_space: SocketAddrSpace,
) -> bool {
    remote_addr.is_ipv6()
        || remote_addr.ip().is_multicast()
        || (matches!(socket_addr_space, SocketAddrSpace::Global)
            && local_ip == Some(remote_addr.ip()))
        || !socket_addr_space.check(&remote_addr)
}

/// AcceptLoop pulls connection attempts off its endpoint, runs the server
/// side of the TLS handshake, then spawns a task that awaits the client's reply.
/// This coarsely bounds the number of cores that can be dedicated
/// to handshake work to the number of accept loops (one per endpoint).
pub(crate) struct AcceptLoop {
    endpoint: Endpoint,
    events_sender: mpsc::Sender<InboundConnectionEvent>,
    stats: Arc<ServerStats>,
    cancel: CancellationToken,
    socket_addr_space: SocketAddrSpace,
    /// Paces how fast this endpoint *starts* handshakes.
    handshake_rate_limiter: TokenBucket,
    /// Bounds the number of in-flight handshakes for this endpoint.
    max_inflight_handshakes: usize,
}

impl AcceptLoop {
    pub(crate) fn new(
        endpoint: Endpoint,
        events_sender: mpsc::Sender<InboundConnectionEvent>,
        stats: Arc<ServerStats>,
        cancel: CancellationToken,
        socket_addr_space: SocketAddrSpace,
        handshake_rate_limiter: TokenBucket,
        max_inflight_handshakes: usize,
    ) -> Self {
        Self {
            endpoint,
            events_sender,
            stats,
            cancel,
            socket_addr_space,
            handshake_rate_limiter,
            max_inflight_handshakes,
        }
    }

    pub(crate) async fn run(self) {
        let Self {
            endpoint,
            events_sender,
            stats,
            cancel,
            socket_addr_space,
            handshake_rate_limiter,
            max_inflight_handshakes,
        } = self;

        // Timer to reopen the admission of Incoming from Endpoint after limiter was exhausted.
        let mut accept_gate = Box::pin(sleep(Duration::ZERO));
        let mut rate_limited = false;

        // In-flight handshake tasks. We use this to be notified whenever any of the
        // per-peer admission tasks complete and to track total count.
        let mut handshakes = JoinSet::new();
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                // Handshake task finished: this potentially reopens the accept arm below.
                Some(joined) = handshakes.join_next(), if !handshakes.is_empty() => {
                    joined.expect("AcceptLoop: handshake task panicked");
                }
                // Rate gate refilled: allow pulling connection attempts.
                _ = &mut accept_gate, if rate_limited => {
                    rate_limited = false;
                }
                // Pull the next attempt only while the rate limit allows and we
                // have a free handshake task slot. We never call `accept()` faster
                // than the limiter permits, nor run more than `max_inflight_handshakes`
                // handshakes at once.
                incoming = endpoint.accept(),
                    if !rate_limited && handshakes.len() < max_inflight_handshakes =>
                {
                    let Some(incoming) = incoming else {
                        info!("Accept loop exiting: endpoint closed.");
                        break;
                    };
                    // We always serve the attempt we already pulled, but we close
                    // the accept gate if we do not have tokens to serve the next one.
                    rate_limited = match handshake_rate_limiter.consume_tokens(1) {
                        Ok(0) => true,
                        Ok(_) => false,
                        Err(_) => {
                            debug_assert!(false, "AcceptLoop woke up too early");
                            true
                        },
                    };
                    if rate_limited {
                        let wait_us = handshake_rate_limiter
                            .us_to_have_tokens(1)
                            .expect("bucket capacity > 1")
                            .saturating_add(1);
                        let deadline = Instant::now()
                            .checked_add(Duration::from_micros(wait_us))
                            .expect("accept-gate deadline should never overflow");
                        accept_gate.as_mut().reset(deadline);
                        stats.handshake_rate_limited.fetch_add(1, Ordering::Relaxed);
                    }
                    let remote_addr = incoming.remote_address();
                    debug!("Incoming connection from {remote_addr}.");
                    if is_invalid_remote_address(
                        remote_addr,
                        incoming.local_ip(),
                        socket_addr_space,
                    ) {
                        incoming.ignore();
                        continue;
                    }
                    // Run the server side of the handshake (CPU-bound crypto).
                    let connecting = match incoming.accept() {
                        Ok(connecting) => connecting,
                        Err(e) => {
                            record_server_error(&Error::from(e), &stats);
                            continue;
                        }
                    };
                    stats.handshakes_started.fetch_add(1, Ordering::Relaxed);
                    // Track the spawned task so the accept guard's `handshakes.len()`
                    // check bounds the in-flight handshakes.
                    handshakes.spawn(wait_for_complete_handshake(
                        connecting,
                        events_sender.clone(),
                        stats.clone(),
                    ));
                }
            }
        }
    }
}

/// Wait for an inbound TLS handshake to complete. This mostly just
/// awaits the client's reply (network-bound), and enforces handshake timeouts.
async fn wait_for_complete_handshake(
    connecting: Connecting,
    events_sender: mpsc::Sender<InboundConnectionEvent>,
    stats: Arc<ServerStats>,
) {
    let connection = match timeout(HANDSHAKE_TIMEOUT, connecting).await {
        Ok(Ok(connection)) => {
            stats.handshakes_completed.fetch_add(1, Ordering::Relaxed);
            connection
        }
        Ok(Err(e)) => {
            record_server_error(&Error::from(e), &stats);
            return;
        }
        // Handshake has timed out
        Err(_elapsed) => {
            stats.handshake_timed_out.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };
    let remote_addr = connection.remote_address();
    let Some(peer) = get_remote_pubkey(&connection) else {
        close_codes::INVALID_IDENTITY.close(&connection);
        record_server_error(&Error::InvalidIdentity(remote_addr), &stats);
        return;
    };
    let _ = events_sender
        .send(InboundConnectionEvent::Accepted { peer, connection })
        .await;
}

/// Per-connection read loop for an accepted inbound connection.
pub(crate) struct ConnectionReader {
    connection: Connection,
    peer: Pubkey,
    remote_addr: SocketAddr,
    ingress: Sender<Datagram>,
    rate_limiter: Arc<TokenBucket>,
    /// Tokens that may remain before shaping kicks in (burst headroom).
    rate_limit_watermark: u64,
    events_sender: mpsc::Sender<InboundConnectionEvent>,
    stats: Arc<ServerStats>,
}

impl ConnectionReader {
    async fn run(self) {
        let Self {
            connection,
            peer,
            remote_addr,
            ingress,
            rate_limiter,
            rate_limit_watermark,
            events_sender,
            stats,
        } = self;
        let stable_id = connection.stable_id();
        loop {
            match connection.read_datagram().await {
                Ok(bytes) => {
                    match rate_limiter.consume_tokens(1) {
                        // normal operation
                        Ok(remaining) if remaining >= rate_limit_watermark => {}
                        // drop excess packets if peer exceeds normal rate
                        Ok(_) => {
                            stats.datagram_rate_limited.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        // peer drained bucket dry - kick them
                        Err(_) => {
                            let _ = events_sender
                                .send(InboundConnectionEvent::FloodDetected { peer })
                                .await;
                            break;
                        }
                    }

                    match ingress.try_send(Datagram {
                        peer_pubkey: peer,
                        peer_address: remote_addr,
                        message: bytes,
                    }) {
                        Ok(()) => {
                            stats.datagrams_received.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(TrySendError::Full(_)) => {
                            stats
                                .datagram_ingress_dropped_channel_full
                                .fetch_add(1, Ordering::Relaxed);
                        }
                        Err(TrySendError::Disconnected(_)) => {
                            debug!("ingress disconnected; reader for {peer} exiting");
                            break;
                        }
                    }
                }
                Err(e) => {
                    // The peer (or we) closed this inbound, or it timed out.
                    record_server_error(&Error::from(e), &stats);
                    break;
                }
            }
        }
        // Send the notification to control that this connection died.
        let _ = events_sender
            .send(InboundConnectionEvent::Closed { peer, stable_id })
            .await;
    }
}

/// Inbound control loop: owns the connection table and registers authenticated
/// connections handed over by [`AcceptLoop`].
pub(crate) struct InboundLoop {
    ingress: Sender<Datagram>,
    /// Temporary per-peer banlist.
    banlist: Banlist<Pubkey>,
    /// Inbound ban commands `(peer, duration)` from the BLS sigverifier.
    ban_receiver: mpsc::Receiver<BanCommand>,
    /// Latest version of the admitted peer list.
    peer_list_receiver: PeerListReceiver,
    /// Identity-rotation notification channel.
    key_updates: KeyUpdateListener,
    /// Endpoints that handle connections. On identity rotation we need to
    /// configure them with the updated TLS config.
    endpoints: Vec<Endpoint>,
    /// Per-peer receive-only connection state.
    peer_state: HashMap<Pubkey, PeerEntry, PubkeyHasherBuilder>,
    /// Tasks reading from opened connections.
    connection_reader_tasks: JoinSet<()>,
    /// Cloned into spawned tasks.
    events_sender: mpsc::Sender<InboundConnectionEvent>,
    /// Channel for read tasks to report their lifetime events.
    events_receiver: mpsc::Receiver<InboundConnectionEvent>,
    stats: Arc<ServerStats>,
    cancel: CancellationToken,
    /// Sustained datagrams-per-second each peer is allowed to send.
    max_datagrams_per_second_per_peer: usize,
    /// Burst headroom above the sustained rate, in tokens.
    peer_rate_limit_burst: u64,
    /// Bucket capacity; draining it dry trips flood control.
    peer_rate_limit_burst_dos: u64,
}

impl InboundLoop {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        ingress: Sender<Datagram>,
        ban_receiver: mpsc::Receiver<BanCommand>,
        peer_list_receiver: PeerListReceiver,
        endpoints: Vec<Endpoint>,
        inbound_events_sender: mpsc::Sender<InboundConnectionEvent>,
        inbound_events_receiver: mpsc::Receiver<InboundConnectionEvent>,
        key_updates: KeyUpdateListener,
        stats: Arc<ServerStats>,
        cancel: CancellationToken,
        max_datagrams_per_second_per_peer: usize,
    ) -> Self {
        let tokens_over = |window: Duration| {
            (max_datagrams_per_second_per_peer as f64 * window.as_secs_f64()).ceil() as u64
        };
        let peer_rate_limit_burst = tokens_over(PEER_RATE_LIMIT_BURST_WINDOW).max(1);
        let peer_rate_limit_burst_dos =
            tokens_over(PEER_RATE_LIMIT_DOS_WINDOW).max(peer_rate_limit_burst.saturating_add(1));
        Self {
            ingress,
            banlist: Banlist::default(),
            ban_receiver,
            peer_list_receiver,
            key_updates,
            endpoints,
            peer_state: HashMap::with_hasher(PubkeyHasherBuilder::default()),
            connection_reader_tasks: JoinSet::new(),
            events_sender: inbound_events_sender,
            events_receiver: inbound_events_receiver,
            stats,
            cancel,
            max_datagrams_per_second_per_peer,
            peer_rate_limit_burst,
            peer_rate_limit_burst_dos,
        }
    }

    /// Counts the number of peers from which we have live connections.
    fn total_peers(&self) -> u64 {
        // scan is not ideal but is simple and correct, and we have < 2000 entries
        self.peer_state
            .values()
            .filter(|entry| !entry.connections.is_empty())
            .count() as u64
    }

    pub(crate) async fn run(mut self) {
        let mut metrics = interval(METRICS_INTERVAL);
        metrics.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let mut peer_list_receiver = self.peer_list_receiver.clone();
        let mut identity_receiver = self.key_updates.receiver.clone();

        info!("Votor QUIC transport server ready.");
        loop {
            tokio::select! {
                // Admission and lifecycle events from the accept loops and the
                // per-connection read tasks.
                Some(event) = self.events_receiver.recv() => self.handle_event(event),
                // A peer was banned by the sig-verifier.
                maybe_ban = self.ban_receiver.recv() => {
                    let Some(BanCommand { peer, duration }) = maybe_ban else {
                        error!("InboundLoop: ban_receiver closed while running, exiting.");
                        debug_assert!(false, "ban_receiver closed while running");
                        break;
                    };
                    self.apply_ban(peer, duration);
                }
                // The local identity changed.
                changed = identity_receiver.changed() => {
                    if changed.is_err() {
                        error!("InboundLoop: identity channel closed while running, exiting.");
                        debug_assert!(false, "identity channel closed while running");
                        break;
                    }
                    let keypair = identity_receiver.borrow_and_update().insecure_clone();
                    let server_config = new_server_config(&keypair, self.max_datagrams_per_second_per_peer, self.endpoints.len());
                    for endpoint in &self.endpoints {
                        endpoint.set_server_config(Some(server_config.clone()));
                    }
                    info!("inbound applied new identity {}", keypair.pubkey());

                    let total_closed = self.close_all(close_codes::IDENTITY_CHANGED);
                    self.stats.connection_closed_identity_changed.fetch_add(total_closed, Ordering::Relaxed);
                    info!("InboundLoop: identity changed ({total_closed} connection(s) closed)");
                    // Never blocks, and a dropped ack only matters at shutdown,
                    // when the updater is gone anyway.
                    let _ = self.key_updates.ack.try_send(());
                }
                // The admitted-peer set changed.
                changed = peer_list_receiver.changed() => {
                    if changed.is_err() {
                        // Unreachable: PeerListService exits when we drop the receiver.
                        error!("InboundLoop: peer_list channel closed while running, exiting.");
                        debug_assert!(false, "peer_list channel closed while running");
                        break;
                    }
                    self.close_not_allowed();
                }
                // Take care of metrics and bookkeeping that does not affect liveness.
                _ = metrics.tick() => {
                    debug!("InboundLoop: running bookkeeping tasks");
                    self.stats.report(self.total_peers());
                    self.banlist.prune();
                    // Reclaim empty connection slots
                    let burst_dos = self.peer_rate_limit_burst_dos;
                    self.peer_state.retain(|_, e| {
                        !e.connections.is_empty()
                            || e.rate_limiter.current_tokens() < burst_dos
                    });
                }
                _ = self.cancel.cancelled() => {
                    break
                },
                Some(joined) = self.connection_reader_tasks.join_next() => {
                    joined.expect("InboundLoop: connection reader task panicked");
                }
            }
        }
        // Close every connection gracefully.
        self.close_all(close_codes::NORMAL_CLOSE);
    }

    /// Close every inbound connection and return how many were closed.
    fn close_all(&self, close_code: close_codes::Spec) -> u64 {
        self.peer_state
            .values()
            .flat_map(|entry| entry.connections.as_slice())
            .inspect(|connection| close_code.close(connection))
            .count() as u64
    }

    /// Scans all open connections and closes those whose peer is no longer admitted.
    fn close_not_allowed(&mut self) {
        // Disjoint field borrows so the membership check can read `peer_list_receiver`
        // while iterating `peer_state` mutably.
        let Self {
            peer_state,
            peer_list_receiver,
            stats,
            ..
        } = self;
        // Snapshot the peer list to avoid holding locks.
        let peer_list = peer_list_receiver.borrow().clone();
        let mut closed_not_in_peer_list = 0u64;
        for (peer, entry) in peer_state.iter() {
            if entry.connections.is_empty() || peer_list.peers.contains_key(peer) {
                continue;
            }
            let closed = entry
                .connections
                .iter()
                .inspect(|connection| close_codes::NOT_ADMITTED.close(connection))
                .count() as u64;
            closed_not_in_peer_list = closed_not_in_peer_list.saturating_add(closed);
        }
        stats
            .connection_closed_not_in_peer_list
            .fetch_add(closed_not_in_peer_list, Ordering::Relaxed);
    }

    /// Apply the ban command and close any open connections from that peer.
    fn apply_ban(&mut self, peer: Pubkey, timeout: Duration) {
        self.banlist.ban(peer, timeout);
        if let Some(entry) = self.peer_state.get(&peer) {
            let closed = entry
                .connections
                .iter()
                .inspect(|connection| close_codes::BANNED.close(connection))
                .count() as u64;
            self.stats
                .connection_closed_banned
                .fetch_add(closed, Ordering::Relaxed);
            // the peer_state entries will get cleaned up after their receive tasks join.
        }
    }

    fn handle_event(&mut self, event: InboundConnectionEvent) {
        match event {
            InboundConnectionEvent::Accepted { peer, connection } => {
                self.maybe_admit_connection(peer, connection)
            }
            InboundConnectionEvent::Closed { peer, stable_id } => match self.peer_state.entry(peer)
            {
                Entry::Occupied(mut slot) => {
                    slot.get_mut()
                        .connections
                        .retain(|c| c.stable_id() != stable_id);
                }
                _ => unreachable!("Entry must be in Occupied state"),
            },
            // Flood detected: close all connections but keep the entry as a
            // tombstone so the depleted rate limiter persists on reconnect.
            InboundConnectionEvent::FloodDetected { peer } => {
                match self.peer_state.get_mut(&peer) {
                    Some(entry) => {
                        warn!("Peer {peer} is flooding packets, closing their connections.");
                        let closed = entry.connections.len() as u64;
                        for connection in entry.connections.iter() {
                            close_codes::FLOODING.close(connection);
                        }
                        self.stats
                            .connection_lost
                            .fetch_add(closed, Ordering::Relaxed);
                    }
                    None => unreachable!("Can not detect flooding on non-existing peer"),
                }
            }
        }
    }

    /// Admission checks for a freshly handshaked inbound connection.
    fn maybe_admit_connection(&mut self, peer: Pubkey, connection: Connection) {
        let remote_addr = connection.remote_address();
        if self.banlist.is_banned(&peer) {
            debug!("Banned peer {peer} attempted a connection from {remote_addr}, rejected");
            close_codes::BANNED.close(&connection);
            record_server_error(&Error::Banned(peer), &self.stats);
            return;
        }

        if !self.peer_list_receiver.borrow().peers.contains_key(&peer) {
            debug!("Not admitted peer {peer} attempted a connection from {remote_addr}, rejected");
            close_codes::NOT_ADMITTED.close(&connection);
            record_server_error(&Error::NotAdmitted(peer), &self.stats);
            return;
        }
        let rate_limiter = match self.peer_state.entry(peer) {
            Entry::Vacant(slot) => {
                let rate_limiter = Arc::new(TokenBucket::new(
                    self.peer_rate_limit_burst_dos,
                    self.peer_rate_limit_burst_dos,
                    self.max_datagrams_per_second_per_peer as f64,
                ));
                let mut connections = ArrayVec::new();
                connections.push(connection.clone());
                slot.insert(PeerEntry {
                    connections,
                    rate_limiter: rate_limiter.clone(),
                });
                rate_limiter
            }
            Entry::Occupied(mut slot) => {
                let entry = slot.get_mut();
                match entry.connections.try_push(connection.clone()) {
                    Ok(()) => Arc::clone(&entry.rate_limiter),
                    Err(_) => {
                        debug!(
                            "Could not admit a connection from {peer} ({remote_addr}) - all slots \
                             occupied"
                        );
                        close_codes::TOO_MANY_CONNECTIONS.close(&connection);
                        record_server_error(&Error::TooManyConnections, &self.stats);
                        return;
                    }
                }
            }
        };
        stats::record_connection_count(&self.stats.peak_unique_peers, self.total_peers());
        info!("Admitted connection from {peer}, remote address {remote_addr}");
        // The ConnectionReader reports [`InboundEvent::Closed`] when it exits so
        // we can get notified when that happens and need not retain a handle here.
        self.connection_reader_tasks.spawn(
            ConnectionReader {
                connection,
                peer,
                remote_addr,
                ingress: self.ingress.clone(),
                rate_limiter,
                rate_limit_watermark: self
                    .peer_rate_limit_burst_dos
                    .saturating_sub(self.peer_rate_limit_burst),
                events_sender: self.events_sender.clone(),
                stats: self.stats.clone(),
            }
            .run(),
        );
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            ALPENGLOW_ALPN, HANDSHAKE_BURST, HANDSHAKE_GLOBAL_RATE, MAX_ENDPOINTS,
            MAX_INFLIGHT_HANDSHAKES,
            transport::{compute_max_incoming, new_client_config, new_transport_config},
        },
        quinn::{ClientConfig, IdleTimeout, crypto::rustls::QuicClientConfig},
        solana_keypair::Keypair,
        solana_net_utils::sockets::{bind_to_localhost_async, unique_port_range_for_tests},
        solana_tls_utils::{new_dummy_x509_certificate, tls_client_config_builder},
        std::{net::Ipv4Addr, time::Duration},
        tokio::{spawn, time::sleep},
    };

    #[test]
    fn remote_address_filter_honors_socket_addr_space() {
        let public = SocketAddr::from(([1, 2, 3, 4], 8000));
        let private = SocketAddr::from(([10, 0, 0, 1], 8000));
        let localhost = SocketAddr::from((Ipv4Addr::LOCALHOST, 8000));

        assert!(!is_invalid_remote_address(
            public,
            None,
            SocketAddrSpace::Global,
        ));
        for addr in [private, localhost] {
            assert!(is_invalid_remote_address(
                addr,
                None,
                SocketAddrSpace::Global,
            ));
            assert!(!is_invalid_remote_address(
                addr,
                Some(addr.ip()),
                SocketAddrSpace::Unspecified,
            ));
        }
        assert!(is_invalid_remote_address(
            public,
            Some(public.ip()),
            SocketAddrSpace::Global,
        ));
    }

    /// Connection attempts arriving at a server whose incoming queue is already
    /// full must be dropped with no reply at all, and must not displace attempts
    /// that are already queued.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn saturated_incoming_queue_drops_silently() {
        // The largest supported endpoint count yields the smallest per-endpoint
        // queue, so the test opens as few connections as it can get away with.
        let queue_capacity = compute_max_incoming(MAX_ENDPOINTS);
        let overflow_attempts = 8;

        let mut ports = unique_port_range_for_tests(2);
        let server_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, ports.next().unwrap()));
        let server_kp = Keypair::new();
        // Deliberately no AcceptLoop: nothing drains the queue, so it fills and stays full.
        let server = Endpoint::server(
            new_server_config(&server_kp, 50, MAX_ENDPOINTS),
            server_addr,
        )
        .expect("bind server endpoint");

        let client_kp = Keypair::new();
        let mut client = Endpoint::client(SocketAddr::from((
            Ipv4Addr::LOCALHOST,
            ports.next().unwrap(),
        )))
        .expect("bind client endpoint");
        let mut client_cfg = new_client_config(&client_kp, 50);
        let mut transport = new_transport_config(50);
        // We want to send Initial exactly once and never give up on it. With production
        // timings a client retransmits after ~1s and abandons the attempt after
        // MAX_IDLE_TIMEOUT, either of which could race the assertions below. So we
        // configure the client to have insane RTT estimate and idle timeout.
        transport
            .initial_rtt(Duration::from_secs(30))
            .max_idle_timeout(Some(
                IdleTimeout::try_from(Duration::from_secs(30)).expect("30s fits IdleTimeout"),
            ));
        client_cfg.transport_config(Arc::new(transport));
        client.set_default_client_config(client_cfg);

        // Fill the queue. The endpoint driver sends each Initial without the
        // `Connecting` being polled; we only need to keep them alive, since
        // dropping one would close the attempt.
        let _filling = (0..queue_capacity)
            .map(|_| {
                client
                    .connect(server_addr, "votor")
                    .expect("client connect to fill queue")
            })
            .collect::<Vec<_>>();
        // Let the server's driver enqueue them. Nothing expires or retransmits
        // while we wait, so this bound only has to beat scheduling delay.
        sleep(Duration::from_secs(1)).await;

        // These arrive at a saturated queue. Poll them so we can tell whether the
        // server answered: a refusal resolves them, a silent drop leaves them pending.
        let overflow = (0..overflow_attempts)
            .map(|_| {
                let connecting = client
                    .connect(server_addr, "votor")
                    .expect("client connect to overflow queue");
                spawn(async move { connecting.await.map(|_| ()) })
            })
            .collect::<Vec<_>>();
        sleep(Duration::from_secs(1)).await;
        for (i, handle) in overflow.iter().enumerate() {
            assert!(
                !handle.is_finished(),
                "overflow attempt {i} got a reply from a saturated server; excess Initials must \
                 be dropped not refused",
            );
        }

        // Exactly the attempts that fit should have been queued; the rest were
        // dropped outright. `ignore()` frees the slot without answering the peer.
        let mut queued = 0;
        while let Ok(Some(incoming)) = timeout(Duration::from_millis(100), server.accept()).await {
            incoming.ignore();
            queued += 1;
        }
        assert_eq!(
            queued, queue_capacity,
            "server queued {queued} attempts but max_incoming is {queue_capacity}",
        );

        for handle in overflow {
            handle.abort();
        }
    }

    /// A peer that completes the QUIC Initial but never finishes the handshake
    /// must not pin an in-flight slot indefinitely.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stalled_handshake_reclaimed_by_timeout() {
        let port = unique_port_range_for_tests(1).start;
        let server_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));

        // Server endpoint driven by the accept loop (where the handshake
        // timeout lives). The control loop is not needed: the handshake never
        // completes, so no Accepted event is ever forwarded.
        let server_kp = Keypair::new();
        let server_cfg = new_server_config(&server_kp, 50, 1);
        let endpoint = Endpoint::server(server_cfg, server_addr).expect("bind server endpoint");
        let server_addr = endpoint.local_addr().expect("server local addr");

        // Sized so a never-completing handshake never needs to send.
        let (events_sender, _events_receiver) = mpsc::channel(1);
        let stats = Arc::new(ServerStats::default());
        let cancel = CancellationToken::new();
        let accept = AcceptLoop::new(
            endpoint,
            events_sender,
            stats.clone(),
            cancel.clone(),
            SocketAddrSpace::Unspecified,
            TokenBucket::new(
                HANDSHAKE_BURST,
                HANDSHAKE_BURST,
                HANDSHAKE_GLOBAL_RATE as f64,
            ),
            MAX_INFLIGHT_HANDSHAKES,
        );
        let loop_handle = spawn(accept.run());

        // One-way proxy: forward client->server, drop server->client.
        let proxy = bind_to_localhost_async().await.expect("bind proxy socket");
        let proxy_addr = proxy.local_addr().expect("proxy local addr");
        let proxy_task = spawn(async move {
            let mut buf = [0u8; 2048];
            while let Ok((n, from)) = proxy.recv_from(&mut buf).await {
                // Black-hole the server's replies; relay everything else (the
                // client's Initial and its retransmits) on to the server.
                if from != server_addr {
                    let _ = proxy.send_to(&buf[..n], server_addr).await;
                }
            }
        });

        // Client connects to the proxy, so it sends but never hears back.
        let client_kp = Keypair::new();
        let client_cfg = new_client_config(&client_kp, 50);
        let port = unique_port_range_for_tests(1).start;
        let mut client = Endpoint::client(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
            .expect("bind client endpoint");
        client.set_default_client_config(client_cfg);
        let connecting = client
            .connect(proxy_addr, "votor")
            .expect("client connect to proxy");
        let client_task = spawn(async move {
            let _ = connecting.await;
        });

        // The loop should accept the Initial, start the handshake, then time it
        // out after HANDSHAKE_TIMEOUT despite the client's retransmissions.
        let deadline = HANDSHAKE_TIMEOUT + Duration::from_secs(3);
        let mut waited = Duration::ZERO;
        let step = Duration::from_millis(100);
        while stats.handshake_timed_out.load(Ordering::Relaxed) == 0 && waited < deadline {
            sleep(step).await;
            waited += step;
        }

        assert!(
            stats.handshake_timed_out.load(Ordering::Relaxed) >= 1,
            "stalled handshake was not reclaimed within {deadline:?}",
        );

        cancel.cancel();
        client_task.abort();
        proxy_task.abort();
        let _ = loop_handle.await;
    }

    /// A client that finishes the TLS handshake but presents a certificate chain
    /// from which no pubkey can be extracted (here a two-cert chain) must be
    /// rejected with INVALID_IDENTITY.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handshake_without_extractable_pubkey_is_rejected() {
        let mut ports = unique_port_range_for_tests(2);
        let server_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, ports.next().unwrap()));

        let server_kp = Keypair::new();
        let server_cfg = new_server_config(&server_kp, 50, 1);
        let server_endpoint =
            Endpoint::server(server_cfg, server_addr).expect("bind server endpoint");

        let (events_sender, mut events_receiver) = mpsc::channel(1);
        let stats = Arc::new(ServerStats::default());
        let cancel = CancellationToken::new();
        let accept = AcceptLoop::new(
            server_endpoint,
            events_sender,
            stats.clone(),
            cancel.clone(),
            SocketAddrSpace::Unspecified,
            TokenBucket::new(
                HANDSHAKE_BURST,
                HANDSHAKE_BURST,
                HANDSHAKE_GLOBAL_RATE as f64,
            ),
            MAX_INFLIGHT_HANDSHAKES,
        );
        let accept_loop_handle = spawn(accept.run());

        // Two-cert chain: the end-entity cert1 matches key1 so CertificateVerify
        // passes and the handshake completes, but get_remote_pubkey rejects any
        // chain whose length != 1.
        let (cert1, key1) = new_dummy_x509_certificate(&Keypair::new());
        let (cert2, _key2) = new_dummy_x509_certificate(&Keypair::new());
        let mut tls = tls_client_config_builder()
            .with_client_auth_cert(vec![cert1, cert2], key1)
            .expect("rustls accepts our two-cert chain");
        tls.alpn_protocols = vec![ALPENGLOW_ALPN.to_vec()];
        let quic_client_config =
            QuicClientConfig::try_from(tls).expect("TLS config should be valid");
        let client_cfg = ClientConfig::new(Arc::new(quic_client_config));

        let mut client = Endpoint::client(SocketAddr::from((
            Ipv4Addr::LOCALHOST,
            ports.next().unwrap(),
        )))
        .expect("bind client endpoint");
        client.set_default_client_config(client_cfg);

        // Try to complete the handshake and hold the connection open.
        spawn(async move {
            if let Ok(connection) = client
                .connect(server_addr, "votor")
                .expect("client connect to server")
                .await
            {
                let _ = connection.closed().await;
            }
        });

        // Poll until the server records the rejection.
        let mut waited = Duration::ZERO;
        let step = Duration::from_millis(100);
        while stats.connection_failed.load(Ordering::Relaxed) == 0 && waited < HANDSHAKE_TIMEOUT {
            sleep(step).await;
            waited += step;
        }

        assert!(
            stats.handshakes_completed.load(Ordering::Relaxed) >= 1,
            "handshake never completed, so the INVALID_IDENTITY branch was not reached",
        );
        assert!(
            stats.connection_failed.load(Ordering::Relaxed) >= 1,
            "server did not reject the pubkey-less client",
        );
        assert!(
            events_receiver.try_recv().is_err(),
            "pubkey-less client must not be Accepted",
        );

        cancel.cancel();
        let _ = accept_loop_handle.await;
    }

    /// A handshake that fails before the timeout (here the client presents no
    /// certificate, which the server's mandatory client auth rejects).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_handshake_failed_not_timed_out() {
        let mut ports = unique_port_range_for_tests(2);
        let server_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, ports.next().unwrap()));

        let server_kp = Keypair::new();
        let server_cfg = new_server_config(&server_kp, 50, 1);
        let endpoint = Endpoint::server(server_cfg, server_addr).expect("bind server endpoint");

        let (events_sender, mut events_receiver) = mpsc::channel(1);
        let stats = Arc::new(ServerStats::default());
        let cancel = CancellationToken::new();
        let accept = AcceptLoop::new(
            endpoint,
            events_sender,
            stats.clone(),
            cancel.clone(),
            SocketAddrSpace::Unspecified,
            TokenBucket::new(
                HANDSHAKE_BURST,
                HANDSHAKE_BURST,
                HANDSHAKE_GLOBAL_RATE as f64,
            ),
            MAX_INFLIGHT_HANDSHAKES,
        );
        let accept_loop_handle = spawn(accept.run());

        // Client presents no certificate. The QUIC Initial is valid so the handshake starts,
        // but the server rejects it.
        let mut tls = tls_client_config_builder().with_no_client_auth();
        tls.alpn_protocols = vec![ALPENGLOW_ALPN.to_vec()];
        let quic = QuicClientConfig::try_from(tls).expect("TLS config should be valid");
        let client_cfg = ClientConfig::new(Arc::new(quic));

        let mut client = Endpoint::client(SocketAddr::from((
            Ipv4Addr::LOCALHOST,
            ports.next().unwrap(),
        )))
        .expect("bind client endpoint");
        client.set_default_client_config(client_cfg);
        spawn(
            client
                .connect(server_addr, "votor")
                .expect("client connect to server"),
        );

        // record_server_error routes the ConnectionError to connection_failed or connection_lost.
        let recorded = |stats: &ServerStats| {
            stats.connection_failed.load(Ordering::Relaxed)
                + stats.connection_lost.load(Ordering::Relaxed)
        };
        let mut waited = Duration::ZERO;
        let step = Duration::from_millis(100);
        while recorded(&stats) == 0 && waited < HANDSHAKE_TIMEOUT {
            sleep(step).await;
            waited += step;
        }

        assert!(
            recorded(&stats) >= 1,
            "server did not record the failed handshake",
        );
        assert!(
            stats.handshakes_started.load(Ordering::Relaxed) >= 1,
            "handshake must have started",
        );
        assert_eq!(
            stats.handshakes_completed.load(Ordering::Relaxed),
            0,
            "handshake must not complete",
        );
        assert_eq!(
            stats.handshake_timed_out.load(Ordering::Relaxed),
            0,
            "failure must be recorded as an error, not a timeout",
        );
        assert!(
            events_receiver.try_recv().is_err(),
            "a failed handshake must not be Accepted",
        );

        cancel.cancel();
        let _ = accept_loop_handle.await;
    }
}
