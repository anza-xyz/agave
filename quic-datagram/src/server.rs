//! Inbound (server) direction: we-accept, receive-only.

use {
    crate::{
        ALLOWLIST_CHECK_INTERVAL, ALPENGLOW_ALPN, BANLIST_PRUNE_INTERVAL, HANDSHAKE_BURST,
        HANDSHAKE_GLOBAL_RATE, HANDSHAKE_TIMEOUT, MAX_INBOUND_CONNECTIONS_PER_PEER,
        MAX_INFLIGHT_HANDSHAKES, METRICS_INTERVAL, PEER_RATE_LIMIT_BURST,
        PEER_RATE_LIMIT_BURST_DOS,
        allowlist::Allowlist,
        close_codes,
        endpoint::Datagram,
        error::Error,
        stats::{self, QuicDatagramStats, record_error},
        transport::{IdentitySnapshot, new_server_config},
    },
    arrayvec::ArrayVec,
    crossbeam_channel::{Sender, TrySendError},
    futures::{StreamExt as _, stream::FuturesUnordered},
    log::{debug, info, warn},
    quinn::{Connecting, Connection, ConnectionError, Endpoint, Incoming},
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

/// State for one peer
pub(crate) struct PeerEntry {
    connections: ArrayVec<Connection, MAX_INBOUND_CONNECTIONS_PER_PEER>,
    rate_limiter: Arc<TokenBucket>,
}

/// Event reported to the InboundLoop. All identity-rotation handling lives in
/// [`AcceptLoop`]; the only rotation signal that reaches here is
/// [`InboundEvent::IdentityRotated`], so this loop carries no generation state.
pub(crate) enum InboundEvent {
    /// A TLS handshake completed and yielded an authenticated peer. The accept
    /// loop has already dropped any handshake that completed across a rotation,
    /// so anything that arrives here is current.
    Accepted {
        peer: Pubkey,
        connection: Connection,
    },
    /// An inbound connection terminated. Reaped by `stable_id`, which quinn
    /// guarantees unique per connection, so a late report is a harmless no-op.
    Closed { peer: Pubkey, stable_id: usize },
    /// The ingress traffic shaping bucket was drained by a sustained flood.
    FloodDetected { peer: Pubkey },
    /// The local identity rotated; evict the table so peers re-handshake under
    /// the new cert. The matching server-config swap happens in [`AcceptLoop`].
    IdentityRotated,
}

/// Accept loop: pulls connection attempts off every endpoint, and drives the
/// TLS handshakes. `Incoming::accept()` runs the server side of the handshake
/// (ECDHE + certificate signature) synchronously, thus a separate task.
///
/// Several SO_REUSEPORT endpoints can share one accept loop: the kernel
/// load-balances inbound datagrams across the sibling sockets, and a single
/// connection's 4-tuple sticks to whichever socket the kernel hashed it to. We
/// drive `accept()` on all of them through one [`FuturesUnordered`] so the
/// global handshake rate-limiter and the in-flight-handshake cap stay shared
/// across the fan-out, and forward every admitted connection into the one
/// [`InboundLoop`] table.
pub(crate) struct AcceptLoop {
    endpoints: Vec<Endpoint>,
    /// Identity-rotation counter
    generation: u64,
    identity_rx: watch::Receiver<Option<Arc<IdentitySnapshot>>>,
    events_tx: mpsc::Sender<InboundEvent>,
    stats: Arc<QuicDatagramStats>,
    shutdown: CancellationToken,
}

/// Pull the next connection attempt off endpoint `idx`, tagging the result with
/// `idx` so the loop knows which endpoint to re-arm. Borrows the endpoint for
/// the lifetime of the returned future; the endpoints live in the loop's owned
/// `Vec`, so the (shared) borrow coexists with the per-rotation
/// `set_server_config` sweep.
async fn accept_next(idx: usize, endpoint: &Endpoint) -> (usize, Option<Incoming>) {
    (idx, endpoint.accept().await)
}

impl AcceptLoop {
    pub(crate) fn new(
        endpoints: Vec<Endpoint>,
        identity_rx: watch::Receiver<Option<Arc<IdentitySnapshot>>>,
        events_tx: mpsc::Sender<InboundEvent>,
        stats: Arc<QuicDatagramStats>,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            endpoints,
            generation: 0,
            identity_rx,
            events_tx,
            stats,
            shutdown,
        }
    }

    pub(crate) async fn run(self) {
        // Destructure so the accept futures can hold shared borrows of
        // `endpoints` while the rest of the state is mutated freely.
        let Self {
            endpoints,
            mut generation,
            mut identity_rx,
            events_tx,
            stats,
            shutdown,
        } = self;

        // Paces how fast we *start* handshakes across all peers and all
        // endpoints (one shared bucket, not one per endpoint).
        let handshake_limiter =
            TokenBucket::new(HANDSHAKE_BURST, HANDSHAKE_BURST, HANDSHAKE_GLOBAL_RATE);
        // Accept gate: while closed the accept arm is disabled for every
        // endpoint. The timer arm re-opens it once the limiter has refilled.
        let mut accept_gate = Box::pin(sleep(Duration::ZERO));
        let mut accept_allowed = true;

        // Stores all in-flight handshakes for polling
        let mut handshakes = FuturesUnordered::new();

        // One in-flight `accept()` per endpoint. A resolved accept is re-armed
        // for its endpoint; while the accept arm is gated off the resolved
        // futures simply sit unpolled here, so excess Incomings stay buffered in
        // quinn exactly as with a single endpoint.
        let mut accepts: FuturesUnordered<_> = endpoints
            .iter()
            .enumerate()
            .map(|(idx, endpoint)| accept_next(idx, endpoint))
            .collect();

        // TODO: this flag is a workaround for some local-cluster tests that are a
        // nightmare to refactor. But they really should be.
        let mut id_closed = false;
        loop {
            tokio::select! {
                biased;
                changed = identity_rx.changed(), if !id_closed => {
                    if changed.is_err() {
                        warn!("identity rotation channel closed; accept loop running without rotation support");
                        id_closed = true;
                        continue;
                    }
                    let snap = identity_rx.borrow_and_update().clone();
                    if let Some(snap) = snap {
                        // Swap every endpoint's server TLS config to the rotated
                        // identity and bump the generation so subsequent
                        // handshakes are stamped with it.
                        let server_config = new_server_config(
                            snap.cert.clone(),
                            snap.key.clone_key(),
                            ALPENGLOW_ALPN,
                        );
                        for endpoint in &endpoints {
                            endpoint.set_server_config(Some(server_config.clone()));
                        }
                        generation = generation.wrapping_add(1);
                        info!(
                            "accept loop applied identity rotation to {} across {} endpoint(s)",
                            snap.pubkey,
                            endpoints.len(),
                        );
                        // Tell the control loop to evict the table so peers
                        // re-handshake under the new cert.
                        let _ = events_tx.send(InboundEvent::IdentityRotated).await;
                    }
                }
                // A handshake finished, failed, or timed out. Forward or discard.
                Some((stamp, outcome)) = handshakes.next(), if !handshakes.is_empty() => {
                    process_handshake_result(stamp, generation, outcome, &events_tx, &stats).await;
                }
                // Rate gate refilled: resume pulling connection attempts.
                _ = &mut accept_gate, if !accept_allowed => {
                    accept_allowed = true;
                }
                // Pull a new connection attempt off whichever endpoint has one
                // ready, only while the rate gate is open and we have handshake
                // headroom; otherwise the Incoming stays buffered in quinn.
                Some((idx, maybe_incoming)) = accepts.next(),
                    if accept_allowed && handshakes.len() < MAX_INFLIGHT_HANDSHAKES =>
                {
                    let Some(incoming) = maybe_incoming else {
                        // Endpoint `idx` closed; stop polling it. Real shutdown
                        // arrives via the cancellation token below.
                        continue;
                    };
                    // Re-arm this endpoint before doing any handshake work.
                    accepts.push(accept_next(idx, &endpoints[idx]));
                    if let Some(connecting) = accept_incoming(incoming, &stats) {
                        stats.handshakes_started.fetch_add(1, Ordering::Relaxed);
                        // Stamp with the generation at accept time so a handshake
                        // that completes across an identity rotation is rejected.
                        let stamp = generation;
                        handshakes.push(async move {
                            (stamp, timeout(HANDSHAKE_TIMEOUT, connecting).await)
                        });
                        if handshake_limiter.consume_tokens(1).is_err() {
                            let wait_us = handshake_limiter.us_to_have_tokens(1).unwrap_or(1000);
                            let deadline = Instant::now()
                                .checked_add(Duration::from_micros(wait_us))
                                .expect("accept-gate deadline should never overflow");
                            accept_gate.as_mut().reset(deadline);
                            accept_allowed = false;
                            stats
                                .handshake_rate_limited
                                .fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                _ = shutdown.cancelled() => break,
            }
        }
    }
}

/// Cheap pre-handshake gates. Returns the in-progress handshake to drive, or
/// `None` if the attempt was shed. Sheds are always `ignore()` (silent, no
/// reply) and never `refuse()`, so a spoofed source address can't make us
/// reflect a CONNECTION_CLOSE back at a victim.
fn accept_incoming(incoming: Incoming, stats: &QuicDatagramStats) -> Option<Connecting> {
    let remote_addr = incoming.remote_address();
    if remote_addr.is_ipv6() || remote_addr.ip().is_multicast() {
        incoming.ignore();
        return None;
    }
    match incoming.accept() {
        Ok(connecting) => Some(connecting),
        Err(e) => {
            record_error(&Error::from(e), stats);
            None
        }
    }
}

/// A handshake finished, failed, or timed out. On success, extract the attested
/// pubkey and forward to the control loop for admission. `stamp` is the
/// generation in force when the handshake was accepted; if it no longer matches
/// `current_stamp`, the local identity rotated mid-handshake and the connection
/// authenticated under our previous cert, so we reject it here rather than
/// forwarding it.
async fn process_handshake_result(
    stamp: u64,
    current_stamp: u64,
    outcome: Result<Result<Connection, ConnectionError>, tokio::time::error::Elapsed>,
    events_tx: &mpsc::Sender<InboundEvent>,
    stats: &QuicDatagramStats,
) {
    let connection = match outcome {
        Ok(Ok(connection)) => {
            stats.handshakes_completed.fetch_add(1, Ordering::Relaxed);
            connection
        }
        Ok(Err(e)) => {
            record_error(&Error::from(e), stats);
            return;
        }
        // Handshake has timed out
        Err(_elapsed) => {
            stats.handshake_timed_out.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };
    if stamp != current_stamp {
        close_codes::IDENTITY_ROTATED.close(&connection);
        stats
            .connection_evicted_identity_rotated
            .fetch_add(1, Ordering::Relaxed);
        return;
    }
    let remote_addr = connection.remote_address();
    let Some(peer) = get_remote_pubkey(&connection) else {
        close_codes::INVALID_IDENTITY.close(&connection);
        record_error(&Error::InvalidIdentity(remote_addr), stats);
        return;
    };
    let _ = events_tx
        .send(InboundEvent::Accepted { peer, connection })
        .await;
}

/// Per-connection read loop for an accepted inbound connection.
pub(crate) struct ConnectionReader {
    pub(crate) connection: Connection,
    pub(crate) peer: Pubkey,
    pub(crate) remote_addr: SocketAddr,
    pub(crate) ingress: Sender<Datagram>,
    pub(crate) banlist: Arc<Banlist<Pubkey>>,
    pub(crate) rate_limiter: Arc<TokenBucket>,
    pub(crate) events: mpsc::Sender<InboundEvent>,
    pub(crate) stats: Arc<QuicDatagramStats>,
}

impl ConnectionReader {
    async fn run(self) {
        let Self {
            connection,
            peer,
            remote_addr,
            ingress,
            banlist,
            rate_limiter,
            events,
            stats,
        } = self;
        let stable_id = connection.stable_id();
        // Use the same bucket to be reused for both shaping and flood control
        const RATE_LIMIT_WATERMARK: u64 = PEER_RATE_LIMIT_BURST_DOS - PEER_RATE_LIMIT_BURST;
        loop {
            match connection.read_datagram().await {
                Ok(bytes) => {
                    // Banlist check happens AFTER the read so a ban that lands
                    // while we're awaiting can't let a follow-up datagram leak
                    // through to ingress.
                    if banlist.is_banned(&peer) {
                        close_codes::BANNED.close(&connection);
                        break;
                    }
                    match rate_limiter.consume_tokens(1) {
                        // normal operation
                        Ok(remaining) if remaining > RATE_LIMIT_WATERMARK => {}
                        // drop excess packets if peer exceeds normal rate
                        Ok(_) => {
                            drop(bytes);
                            stats.datagram_rate_limited.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        // peer drained bucket dry - kick them
                        Err(_) => {
                            drop(bytes);
                            let _ = events.send(InboundEvent::FloodDetected { peer }).await;
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
                    record_error(&Error::from(e), &stats);
                    break;
                }
            }
        }
        // Send the notification to control that this connection died.
        let _ = events.send(InboundEvent::Closed { peer, stable_id }).await;
    }
}

/// Inbound control loop: owns the connection table and admits authenticated
/// connections handed over by [`AcceptLoop`]. Does no handshake work itself.
pub(crate) struct InboundLoop {
    pub(crate) ingress: Sender<Datagram>,
    /// Policy to instantly ban all packets from a Pubkey
    pub(crate) banlist: Arc<Banlist<Pubkey>>,
    /// Policy for which peers may occupy a slot or retain a connection.
    pub(crate) allowlist: Arc<dyn Allowlist>,
    /// Per-peer accepted receive-only connection state, owned solely by this
    /// loop.
    pub(crate) peer_state: HashMap<Pubkey, PeerEntry, PubkeyHasherBuilder>,
    /// Channel for read tasks to report their lifetime events.
    pub(crate) events_tx: mpsc::Sender<InboundEvent>,
    /// Channel for read tasks to report their lifetime events.
    pub(crate) events_rx: mpsc::Receiver<InboundEvent>,
    pub(crate) stats: Arc<QuicDatagramStats>,
    pub(crate) shutdown: CancellationToken,
    /// Sustained datagrams-per-second each peer is allowed to send.
    pub(crate) max_datagrams_per_second_per_peer: f64,
}

impl InboundLoop {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        ingress: Sender<Datagram>,
        banlist: Arc<Banlist<Pubkey>>,
        allowlist: Arc<dyn Allowlist>,
        events_tx: mpsc::Sender<InboundEvent>,
        events_rx: mpsc::Receiver<InboundEvent>,
        stats: Arc<QuicDatagramStats>,
        shutdown: CancellationToken,
        max_datagrams_per_second_per_peer: f64,
    ) -> Self {
        Self {
            ingress,
            banlist,
            allowlist,
            peer_state: HashMap::with_hasher(PubkeyHasherBuilder::default()),
            events_tx,
            events_rx,
            stats,
            shutdown,
            max_datagrams_per_second_per_peer,
        }
    }

    /// Live inbound connections (each pubkey may hold several).
    fn connection_count(&self) -> u64 {
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
        prune.set_missed_tick_behavior(MissedTickBehavior::Skip);

        let mut metrics = interval(METRICS_INTERVAL);
        metrics.set_missed_tick_behavior(MissedTickBehavior::Skip);

        // Periodic table-wide admission sweep for dormant peers
        let mut allowlist_check = interval(ALLOWLIST_CHECK_INTERVAL);
        allowlist_check.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                biased;
                // Admission, lifecycle, and rotation events from the accept loop
                // and the per-connection read tasks.
                Some(event) = self.events_rx.recv() => self.handle_event(event),
                // Metrics are quite handy to have even if we are flooded with incoming.
                _ = metrics.tick() => stats::report_server(&self.stats, self.connection_count()),
                // Close connections whose peer fell out of the allowlist or
                // landed on the banlist while idle. One pass over the table,
                // one membership check per peer.
                _ = allowlist_check.tick() => self.evict_disallowed(),
                // When idle we can take care of bookkeeping.
                _ = prune.tick() => {
                    self.banlist.prune();
                    // Reclaim empty connection slots
                    self.peer_state.retain(|_, e| {
                        !e.connections.is_empty()
                            || e.rate_limiter.current_tokens() < PEER_RATE_LIMIT_BURST_DOS
                    });
                }
                // Shutdown is never done in a hurry
                _ = self.shutdown.cancelled() => break,
            }
        }
    }

    /// Evict the whole inbound table so peers re-handshake under the new
    /// identity. Driven by [`InboundEvent::IdentityRotated`] from [`AcceptLoop`],
    /// which owns the matching server-TLS-config swap.
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

    /// Periodic table-wide admission sweep. Closes every connection whose peer
    /// has dropped out of the allowlist or landed on the banlist while idle.
    ///
    /// Connections are drained but the (now empty) `PeerEntry` is kept so the
    /// peer's rate limiter survives a reconnect.
    fn evict_disallowed(&mut self) {
        // Disjoint field borrows so the membership checks can read `allowlist`
        // and `banlist` while iterating `peer_state` mutably.
        let Self {
            peer_state,
            allowlist,
            banlist,
            stats,
            ..
        } = self;
        let mut evicted_allowlist = 0u64;
        for (peer, entry) in peer_state.iter_mut() {
            if entry.connections.is_empty() {
                continue;
            }
            if banlist.is_banned(peer) {
                for connection in entry.connections.drain(..) {
                    close_codes::BANNED.close(&connection);
                }
            } else if !allowlist.allow(peer) {
                let closed = entry
                    .connections
                    .drain(..)
                    .inspect(|connection| close_codes::NOT_ADMITTED.close(connection))
                    .count() as u64;
                evicted_allowlist = evicted_allowlist.saturating_add(closed);
            }
        }
        stats
            .connection_evicted_allowlist
            .fetch_add(evicted_allowlist, Ordering::Relaxed);
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
            InboundEvent::IdentityRotated => self.evict_all(),
        }
    }

    /// Admission checks for a freshly handshaked inbound connection.
    fn maybe_admit_connection(&mut self, peer: Pubkey, connection: Connection) {
        if self.banlist.is_banned(&peer) {
            close_codes::BANNED.close(&connection);
            record_error(&Error::Banned(peer), &self.stats);
            return;
        }
        if !self.allowlist.allow(&peer) {
            close_codes::NOT_ADMITTED.close(&connection);
            record_error(&Error::NotAdmitted(peer), &self.stats);
            return;
        }
        let remote_addr = connection.remote_address();
        let rate_limiter = match self.peer_state.entry(peer) {
            Entry::Vacant(slot) => {
                let rate_limiter = Arc::new(TokenBucket::new(
                    PEER_RATE_LIMIT_BURST_DOS,
                    PEER_RATE_LIMIT_BURST_DOS,
                    self.max_datagrams_per_second_per_peer,
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
                        close_codes::TABLE_FULL.close(&connection);
                        record_error(&Error::TableFull, &self.stats);
                        return;
                    }
                }
            }
        };
        self.stats.record_connection_count(self.connection_count());
        // The read loop reports [`InboundEvent::Closed`] when it exits so this
        // loop can reap the table slot.
        spawn(
            ConnectionReader {
                connection,
                peer,
                remote_addr,
                ingress: self.ingress.clone(),
                banlist: self.banlist.clone(),
                rate_limiter,
                events: self.events_tx.clone(),
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
        crate::transport::new_client_config,
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
    /// its Initial (because it never hears a reply) would defeat a pure idle
    /// timeout. This drives exactly that case through a one-way proxy that
    /// black-holes server->client traffic, and asserts the accept loop reclaims
    /// the handshake via `HANDSHAKE_TIMEOUT` (the `handshake_timed_out` counter).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stalled_handshake_reclaimed_by_timeout() {
        let loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));

        // --- Server endpoint driven by the accept loop (where the handshake
        // timeout lives). The control loop is not needed: the handshake never
        // completes, so no Accepted event is ever forwarded. ---
        let server_kp = Keypair::new();
        let id = IdentitySnapshot::from_keypair(&server_kp);
        let server_cfg = new_server_config(id.cert, id.key, ALPENGLOW_ALPN);
        let endpoint = Endpoint::server(server_cfg, loopback).unwrap();
        let server_addr = endpoint.local_addr().unwrap();

        // Keep the sender alive so the loop keeps rotation support enabled; we
        // never rotate in this test.
        let (_identity_tx, identity_rx) = watch::channel(None);
        // Sized so a never-completing handshake never needs to send.
        let (events_tx, _events_rx) = mpsc::channel(1);
        let stats = Arc::new(QuicDatagramStats::default());
        let shutdown = CancellationToken::new();
        let accept = AcceptLoop::new(
            vec![endpoint],
            identity_rx,
            events_tx,
            stats.clone(),
            shutdown.clone(),
        );
        let loop_handle = spawn(accept.run());

        // --- One-way proxy: forward client->server, drop server->client. ---
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

        // --- Client connects to the proxy, so it sends but never hears back. ---
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
