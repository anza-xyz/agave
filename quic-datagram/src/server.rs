//! Inbound (server) direction: we-accept, receive-only.

use {
    crate::{
        ALPENGLOW_ALPN, BANLIST_PRUNE_INTERVAL, HANDSHAKE_TIMEOUT,
        MAX_INBOUND_CONNECTIONS_PER_PEER, METRICS_INTERVAL, PEER_RATE_LIMIT_BURST_WINDOW,
        PEER_RATE_LIMIT_DOS_WINDOW, PeerListReceiver, close_codes,
        endpoint::{BanCommand, Datagram},
        error::Error,
        stats::{self, ServerStats, record_server_error},
        transport::{IdentitySnapshot, new_server_config},
    },
    arrayvec::ArrayVec,
    crossbeam_channel::{Sender, TrySendError},
    futures::{StreamExt as _, stream::FuturesUnordered},
    log::{debug, info, warn},
    quinn::{Connecting, Connection, Endpoint},
    solana_net_utils::{banlist::Banlist, token_bucket::TokenBucket},
    solana_pubkey::{Pubkey, PubkeyHasherBuilder},
    solana_tls_utils::get_remote_pubkey,
    std::{
        collections::{HashMap, hash_map::Entry},
        net::SocketAddr,
        sync::{Arc, atomic::Ordering},
        time::Duration,
    },
    tokio::{
        spawn,
        sync::{mpsc, watch},
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
pub(crate) enum InboundEvent {
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

/// AcceptLoop pulls connection attempts off its endpoint,
/// runs the server side of the TLS handshake, then
/// spawns a task that awaits the client's reply.
/// This coarsely bounds the number of cores that can be dedicated
/// to handshake work to the number of accept loops (one per endpoint).
pub(crate) struct AcceptLoop {
    endpoint: Endpoint,
    identity_receiver: watch::Receiver<Option<Arc<IdentitySnapshot>>>,
    events_sender: mpsc::Sender<InboundEvent>,
    stats: Arc<ServerStats>,
    shutdown: CancellationToken,
    /// Paces how fast this endpoint *starts* handshakes.
    limiter: TokenBucket,
    /// Bounds the number of in-flight handshakes for this endpoint.
    max_inflight_handshakes: usize,
}

impl AcceptLoop {
    pub(crate) fn new(
        endpoint: Endpoint,
        identity_receiver: watch::Receiver<Option<Arc<IdentitySnapshot>>>,
        events_sender: mpsc::Sender<InboundEvent>,
        stats: Arc<ServerStats>,
        shutdown: CancellationToken,
        handshake_rate_limiter: TokenBucket,
        max_inflight_handshakes: usize,
    ) -> Self {
        Self {
            endpoint,
            identity_receiver,
            events_sender,
            stats,
            shutdown,
            limiter: handshake_rate_limiter,
            max_inflight_handshakes,
        }
    }

    pub(crate) async fn run(self) {
        let Self {
            endpoint,
            mut identity_receiver,
            events_sender,
            stats,
            shutdown,
            limiter,
            max_inflight_handshakes,
        } = self;

        // Timer to reopen the admission of Incoming from Endpoint after limiter was exhausted.
        let mut accept_gate = Box::pin(sleep(Duration::ZERO));
        let mut rate_limited = false;

        // In-flight handshake tasks. We use this to be notified whenever any of the
        // per-peer admission tasks complete and to track total count.
        let mut handshakes = FuturesUnordered::new();

        loop {
            tokio::select! {
                biased;
                // Identity rotation: swap this endpoint's server config so new
                // handshakes observe the new cert. In-flight handshakes that
                // complete under the old cert are harmless and ignored here.
                changed = identity_receiver.changed() => {
                    if changed.is_err() {
                        info!("identity rotation channel closed; accept loop exiting");
                        break;
                    }
                    if let Some(new_identity) = identity_receiver.borrow_and_update().clone() {
                        let server_config = new_server_config(
                            new_identity.cert.clone(),
                            new_identity.key.clone_key(),
                            ALPENGLOW_ALPN,
                        );
                        endpoint.set_server_config(Some(server_config));
                        info!("AcceptLoop applied new identity {}", new_identity.pubkey);
                    }
                }
                // Handshake task finished: this potentially reopens the accept arm below.
                Some(_joined) = handshakes.next(), if !handshakes.is_empty() => {}
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
                        break; // Endpoint closed
                    };
                    // Handshake ratelimiter check - stop admitting tasks once limiter
                    // is exhausted.
                    if limiter.consume_tokens(1).is_err() {
                        incoming.ignore();
                        let wait_us = limiter.us_to_have_tokens(1).unwrap_or(1000);
                        let deadline = Instant::now()
                            .checked_add(Duration::from_micros(wait_us))
                            .expect("accept-gate deadline should never overflow");
                        accept_gate.as_mut().reset(deadline);
                        rate_limited = true;
                        stats.handshake_rate_limited.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    let remote_addr = incoming.remote_address();
                    if remote_addr.is_ipv6() || remote_addr.ip().is_multicast() {
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
                    handshakes.push(spawn(Self::wait_for_complete_handshake(
                        connecting,
                        events_sender.clone(),
                        stats.clone(),
                    )));
                }
                _ = shutdown.cancelled() => break,
            }
        }
    }

    /// Wait for an inbound TLS handshake to complete. This mostly just
    /// awaits the client's reply (network-bound), and enforces handshake timeouts.
    async fn wait_for_complete_handshake(
        connecting: Connecting,
        events_sender: mpsc::Sender<InboundEvent>,
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
            .send(InboundEvent::Accepted { peer, connection })
            .await;
    }
}

/// Per-connection read loop for an accepted inbound connection.
pub(crate) struct ConnectionReader {
    pub(crate) connection: Connection,
    pub(crate) peer: Pubkey,
    pub(crate) remote_addr: SocketAddr,
    pub(crate) ingress: Sender<Datagram>,
    pub(crate) rate_limiter: Arc<TokenBucket>,
    /// Tokens that may remain before shaping kicks in (burst headroom).
    pub(crate) rate_limit_watermark: u64,
    pub(crate) events_sender: mpsc::Sender<InboundEvent>,
    pub(crate) stats: Arc<ServerStats>,
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
                                .send(InboundEvent::FloodDetected { peer })
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
                    // Record and exit; the control loop reaps the table slot
                    // from the `Closed` event below.
                    record_server_error(&Error::from(e), &stats);
                    break;
                }
            }
        }
        // Send the notification to control that this connection died.
        let _ = events_sender
            .send(InboundEvent::Closed { peer, stable_id })
            .await;
    }
}

/// Inbound control loop: owns the connection table and registers authenticated
/// connections handed over by [`AcceptLoop`].
pub(crate) struct InboundLoop {
    pub(crate) ingress: Sender<Datagram>,
    /// Temporary per-peer banlist.
    pub(crate) banlist: Banlist<Pubkey>,
    /// Inbound ban commands `(peer, duration)` from the BLS sigverifier.
    pub(crate) ban_receiver: mpsc::Receiver<BanCommand>,
    /// Latest admitted-peer snapshot, or `None` to admit all peers (dev/test).
    /// A change publishes a new snapshot and triggers an eviction sweep.
    pub(crate) peer_list_receiver: Option<PeerListReceiver>,
    /// Identity-rotation channel. On change the whole table is evicted so peers
    /// re-handshake.
    pub(crate) identity_receiver: watch::Receiver<Option<Arc<IdentitySnapshot>>>,
    /// Per-peer receive-only connection state.
    pub(crate) peer_state: HashMap<Pubkey, PeerEntry, PubkeyHasherBuilder>,
    /// Cloned into spawned tasks.
    pub(crate) events_sender: mpsc::Sender<InboundEvent>,
    /// Channel for read tasks to report their lifetime events.
    pub(crate) events_receiver: mpsc::Receiver<InboundEvent>,
    pub(crate) stats: Arc<ServerStats>,
    pub(crate) shutdown: CancellationToken,
    /// Sustained datagrams-per-second each peer is allowed to send.
    pub(crate) max_datagrams_per_second_per_peer: usize,
    /// Burst headroom above the sustained rate, in tokens.
    pub(crate) peer_rate_limit_burst: u64,
    /// Bucket capacity; draining it dry trips flood control.
    pub(crate) peer_rate_limit_burst_dos: u64,
}

impl InboundLoop {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        ingress: Sender<Datagram>,
        ban_receiver: mpsc::Receiver<BanCommand>,
        peer_list_receiver: Option<PeerListReceiver>,
        inbound_events_sender: mpsc::Sender<InboundEvent>,
        inbound_events_receiver: mpsc::Receiver<InboundEvent>,
        identity_receiver: watch::Receiver<Option<Arc<IdentitySnapshot>>>,
        stats: Arc<ServerStats>,
        shutdown: CancellationToken,
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
            identity_receiver,
            peer_state: HashMap::with_hasher(PubkeyHasherBuilder::default()),
            events_sender: inbound_events_sender,
            events_receiver: inbound_events_receiver,
            stats,
            shutdown,
            max_datagrams_per_second_per_peer,
            peer_rate_limit_burst,
            peer_rate_limit_burst_dos,
        }
    }

    /// Live inbound connections (each pubkey may hold several).
    fn count_connections(&self) -> u64 {
        self.peer_state
            .values()
            .map(|e| e.connections.len())
            .sum::<usize>() as u64
    }

    /// Remove the connection with the given `stable_id` from `peer`'s inbound
    /// set. Keeps the `PeerEntry` so the rate limiter state survives a reconnect.
    /// Entries are reclaimed by the prune task once the rate limiter has refilled.
    fn reap_connection(&mut self, peer: &Pubkey, stable_id: usize) {
        if let Entry::Occupied(mut slot) = self.peer_state.entry(*peer) {
            slot.get_mut()
                .connections
                .retain(|c| c.stable_id() != stable_id);
        }
    }

    pub(crate) async fn run(mut self) {
        let mut prune = interval(BANLIST_PRUNE_INTERVAL);
        prune.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let mut metrics = interval(METRICS_INTERVAL);
        metrics.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let mut peer_list_receiver = self.peer_list_receiver.clone();
        let mut identity_receiver = self.identity_receiver.clone();

        loop {
            tokio::select! {
                biased;
                // Admission and lifecycle events from the accept loops and the
                // per-connection read tasks.
                Some(event) = self.events_receiver.recv() => self.handle_event(event),
                // A peer was banned by the sig-verifier.
                Some(BanCommand { peer, duration }) = self.ban_receiver.recv() => self.apply_ban(peer, duration),
                // The local identity rotated: evict the table so peers
                // re-handshake under the new cert.
                changed = identity_receiver.changed() => {
                    if changed.is_err() {
                        info!("identity rotation channel closed; inbound loop exiting");
                        break;
                    }
                    let _ = identity_receiver.borrow_and_update();
                    self.evict_all();
                }
                // The admitted-peer set changed.
                changed = async {
                    match peer_list_receiver.as_mut() {
                        Some(receiver) => receiver.changed().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match changed {
                        Ok(()) => self.evict_not_allowed(),
                        Err(_) => {
                            warn!(
                                "peer_list channel closed; inbound admission frozen at last \
                                 snapshot"
                            );
                            // Disarm: parks the arm via the `None` branch above.
                            peer_list_receiver = None;
                        }
                    }
                }
                // Metrics are quite handy to have even if we are flooded with incoming.
                _ = metrics.tick() => stats::report_server(&self.stats, self.count_connections()),
                // When idle we can take care of bookkeeping.
                _ = prune.tick() => {
                    self.banlist.prune();
                    // Reclaim empty connection slots
                    let burst_dos = self.peer_rate_limit_burst_dos;
                    self.peer_state.retain(|_, e| {
                        !e.connections.is_empty()
                            || e.rate_limiter.current_tokens() < burst_dos
                    });
                }
                // Shutdown is never done in a hurry
                _ = self.shutdown.cancelled() => break,
            }
        }
    }

    /// Evict the whole inbound table so peers re-handshake under the new
    /// identity. Driven by a change on the identity channel; the accept loops
    /// own the matching per-endpoint server-TLS-config swaps.
    fn evict_all(&mut self) {
        let evicted = self
            .peer_state
            .drain()
            .flat_map(|(_, entry)| entry.connections)
            .inspect(|connection| close_codes::IDENTITY_ROTATED.close(connection))
            .count() as u64;
        self.stats
            .connection_evicted_identity_rotated
            .fetch_add(evicted, Ordering::Relaxed);
        info!("inbound identity rotated ({evicted} connection(s) evicted)");
    }

    /// Table-wide admission sweep, driven by a peer_list change. Closes every
    /// connection whose peer is no longer admitted.
    fn evict_not_allowed(&mut self) {
        // Disjoint field borrows so the membership check can read `peer_list_receiver`
        // while iterating `peer_state` mutably.
        let Self {
            peer_state,
            peer_list_receiver,
            stats,
            ..
        } = self;
        // `None` means admit-all; nothing to evict.
        let Some(peer_list_receiver) = peer_list_receiver.as_ref() else {
            return;
        };
        // Take a snapshow of the peer list to avoid holding locks.
        let peer_list = peer_list_receiver.borrow().clone();
        let mut evicted_peer_list = 0u64;
        for (peer, entry) in peer_state.iter_mut() {
            if entry.connections.is_empty() || peer_list.contains_key(peer) {
                continue;
            }
            let closed = entry
                .connections
                .drain(..)
                .inspect(|connection| close_codes::NOT_ADMITTED.close(connection))
                .count() as u64;
            evicted_peer_list = evicted_peer_list.saturating_add(closed);
        }
        stats
            .connection_evicted_peer_list
            .fetch_add(evicted_peer_list, Ordering::Relaxed);
    }

    /// Apply the ban command and close any open connections
    /// from that peer.
    fn apply_ban(&mut self, peer: Pubkey, timeout: Duration) {
        self.banlist.ban(peer, timeout);
        // if peer has any open connections...
        if let Some(mut entry) = self.peer_state.remove(&peer) {
            // ... we go over them and close each.
            let closed = entry
                .connections
                .drain(..)
                .inspect(|connection| close_codes::BANNED.close(connection))
                .count() as u64;

            self.stats
                .connection_evicted_banned
                .fetch_add(closed, Ordering::Relaxed);
        }
    }

    /// Apply an admission or connection-lifecycle event.
    fn handle_event(&mut self, event: InboundEvent) {
        match event {
            // The accept loop already filtered handshakes that completed across
            // a rotation, so an Accepted that reaches here is current.
            InboundEvent::Accepted { peer, connection } => {
                self.maybe_admit_connection(peer, connection)
            }
            InboundEvent::Closed { peer, stable_id } => self.reap_connection(&peer, stable_id),
            // Flood detected: close all connections but keep the entry as a
            // tombstone so the depleted rate limiter persists on reconnect.
            InboundEvent::FloodDetected { peer } => {
                if let Some(entry) = self.peer_state.get_mut(&peer) {
                    let closed = entry.connections.len() as u64;
                    for connection in entry.connections.drain(..) {
                        close_codes::FLOODING.close(&connection);
                    }
                    self.stats
                        .connection_lost
                        .fetch_add(closed, Ordering::Relaxed);
                }
            }
        }
    }

    /// Admission checks for a freshly handshaked inbound connection.
    fn maybe_admit_connection(&mut self, peer: Pubkey, connection: Connection) {
        if self.banlist.is_banned(&peer) {
            close_codes::BANNED.close(&connection);
            record_server_error(&Error::Banned(peer), &self.stats);
            return;
        }

        if self
            .peer_list_receiver
            .as_ref()
            // not in the peerlist
            .map(|rx| !rx.borrow().contains_key(&peer))
            // or no peerlist provided
            .unwrap_or(false)
        {
            close_codes::NOT_ADMITTED.close(&connection);
            record_server_error(&Error::NotAdmitted(peer), &self.stats);
            return;
        }
        let remote_addr = connection.remote_address();
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
                        close_codes::TOO_MANY_CONNECTIONS.close(&connection);
                        record_server_error(&Error::TableFull, &self.stats);
                        return;
                    }
                }
            }
        };
        stats::record_connection_count(&self.stats.peak_connections, self.count_connections());
        // The read loop reports [`InboundEvent::Closed`] when it exits so this
        // loop can reap the table slot.
        spawn(
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
            HANDSHAKE_BURST, HANDSHAKE_GLOBAL_RATE, MAX_INFLIGHT_HANDSHAKES,
            transport::new_client_config,
        },
        solana_keypair::Keypair,
        std::{
            net::{IpAddr, Ipv4Addr},
            time::Duration,
        },
        tokio::time::sleep,
    };

    /// A peer that completes the QUIC Initial but never finishes the handshake
    /// must not pin an in-flight slot indefinitely. quinn resets the idle timer
    /// on every authenticated packet, so a real client that keeps retransmitting
    /// its Initial (because it never hears a reply) would not trigger idle
    /// timeout. This reproduces the scenario through a one-way proxy that
    /// black-holes server->client traffic, and asserts the accept loop reclaims
    /// the handshake via `HANDSHAKE_TIMEOUT` (the `handshake_timed_out` counter).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stalled_handshake_reclaimed_by_timeout() {
        let loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));

        // Server endpoint driven by the accept loop (where the handshake
        // timeout lives). The control loop is not needed: the handshake never
        // completes, so no Accepted event is ever forwarded.
        let server_kp = Keypair::new();
        let id = IdentitySnapshot::from_keypair(&server_kp);
        let server_cfg = new_server_config(id.cert, id.key, ALPENGLOW_ALPN);
        let endpoint = Endpoint::server(server_cfg, loopback).unwrap();
        let server_addr = endpoint.local_addr().unwrap();

        // Keep the sender alive so the loop keeps rotation support enabled; we
        // never rotate in this test.
        let (_identity_sender, identity_receiver) = watch::channel(None);
        // Sized so a never-completing handshake never needs to send.
        let (events_sender, _events_receiver) = mpsc::channel(1);
        let stats = Arc::new(ServerStats::default());
        let shutdown = CancellationToken::new();
        let accept = AcceptLoop::new(
            endpoint,
            identity_receiver,
            events_sender,
            stats.clone(),
            shutdown.clone(),
            TokenBucket::new(HANDSHAKE_BURST, HANDSHAKE_BURST, HANDSHAKE_GLOBAL_RATE),
            MAX_INFLIGHT_HANDSHAKES,
        );
        let loop_handle = spawn(accept.run());

        // One-way proxy: forward client->server, drop server->client.
        let proxy = solana_net_utils::sockets::bind_to_async(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
            .await
            .unwrap();
        let proxy_addr = proxy.local_addr().unwrap();
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
        let cid = IdentitySnapshot::from_keypair(&client_kp);
        let client_cfg = new_client_config(cid.cert, cid.key, ALPENGLOW_ALPN);
        let mut client = Endpoint::client(loopback).unwrap();
        client.set_default_client_config(client_cfg);
        let connecting = client.connect(proxy_addr, "votor").unwrap();
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

        shutdown.cancel();
        client_task.abort();
        proxy_task.abort();
        let _ = loop_handle.await;
    }
}
