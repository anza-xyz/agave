//! Outbound (client) direction: we initiate, send-only.
use {
    crate::{
        ALPENGLOW_ALPN, METRICS_INTERVAL, PeerListReceiver, close_codes,
        endpoint::ExitSignals,
        error::Error,
        stats::{self, ClientStats, record_client_error},
        transport::{Identity, new_client_config},
    },
    bytes::Bytes,
    log::{debug, error, info, warn},
    quinn::{Connection, Endpoint, SendDatagramError},
    solana_pubkey::{Pubkey, PubkeyHasherBuilder},
    solana_tls_utils::{get_remote_pubkey, socket_addr_to_quic_server_name},
    std::{
        collections::HashMap,
        net::SocketAddr,
        sync::{Arc, atomic::Ordering},
        time::Duration,
    },
    tokio::{
        sync::{mpsc, watch},
        task::JoinSet,
        time::{MissedTickBehavior, interval},
    },
};

/// How often the outbound loop reconciles its connection table against the
/// peer_list. Doubles as the retry interval for failed connects.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(1);

/// State of a peer's entry in the outbound table.
#[derive(Debug)]
pub(crate) enum PeerState {
    /// Connection initiated but is not ready yet.
    Connecting,
    /// Live send-only connection.
    Established {
        connection: Connection,
        // This is cached at install so reconcile can
        // detect an address change without locking quinn state.
        target_address: SocketAddr,
    },
}

/// `Ok` carries the peer and its newly established connection; `Err` carries
/// the peer we tried (and failed) to connect to.
type HandshakeOutcome = Result<(Pubkey, Connection), Pubkey>;

/// Open and authenticate a new connection to `peer` at `addr`.
async fn connect(
    endpoint: Endpoint,
    peer: Pubkey,
    addr: SocketAddr,
    stats: Arc<ClientStats>,
) -> HandshakeOutcome {
    let attempt = async {
        let server_name = socket_addr_to_quic_server_name(addr);
        let connection = endpoint.connect(addr, &server_name)?.await?;
        let attested = get_remote_pubkey(&connection).ok_or(Error::InvalidIdentity(addr))?;
        if attested != peer {
            close_codes::INVALID_IDENTITY.close(&connection);
            return Err(Error::InvalidIdentity(addr));
        }
        Ok(connection)
    };
    match attempt.await {
        Ok(connection) => Ok((peer, connection)),
        Err(e) => {
            warn!("Connection attempt to ({peer}, {addr}) failed: {e:?}");
            record_client_error(&e, &stats);
            Err(peer)
        }
    }
}

/// Outbound control loop for the egress direction (we-connect, send-only).
/// Strives to ensure open connections to everyone in the peer_list.
pub(crate) struct OutboundLoop {
    pub(crate) endpoint: Endpoint,
    pub(crate) local_pubkey: Pubkey,
    /// Channel for outbound messages to be broadcast
    pub(crate) egress_receiver: mpsc::Receiver<Bytes>,
    pub(crate) identity_receiver: watch::Receiver<Option<Arc<Identity>>>,
    pub(crate) peer_list_receiver: PeerListReceiver,
    /// Per-peer send-only connection state.
    /// Size is limited to the peer_list size by reconcile task.
    pub(crate) peer_state: HashMap<Pubkey, PeerState, PubkeyHasherBuilder>,
    pub(crate) in_flight_handshakes: JoinSet<HandshakeOutcome>,
    pub(crate) exit_signals: ExitSignals,
    pub(crate) stats: Arc<ClientStats>,
}

impl OutboundLoop {
    pub(crate) fn new(
        endpoint: Endpoint,
        local_pubkey: Pubkey,
        egress_receiver: mpsc::Receiver<Bytes>,
        identity_receiver: watch::Receiver<Option<Arc<Identity>>>,
        peer_list_receiver: PeerListReceiver,
        exit_signals: ExitSignals,
    ) -> Self {
        Self {
            endpoint,
            local_pubkey,
            egress_receiver,
            identity_receiver,
            peer_list_receiver,
            peer_state: HashMap::with_hasher(PubkeyHasherBuilder::default()),
            in_flight_handshakes: JoinSet::new(),
            exit_signals,
            stats: Arc::default(),
        }
    }

    pub(crate) async fn run(mut self) {
        let mut metrics = interval(METRICS_INTERVAL);
        metrics.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let mut reconcile_timer = interval(RECONCILE_INTERVAL);
        reconcile_timer.set_missed_tick_behavior(MissedTickBehavior::Delay);

        // A separate receiver clone drives change notifications, the
        // reads in `reconcile` go through `self.peer_list_receiver`.
        let mut peer_list_receiver = self.peer_list_receiver.clone();

        info!("Votor QUIC transport client ready.");
        loop {
            tokio::select! {
                changed = self.identity_receiver.changed() => {
                    if changed.is_err(){
                        debug_assert!(
                            self.exit_signals.is_exiting(),
                            "OutboundLoop: identity channel closed while running."
                        );
                        break;
                    }
                    let new_identity = self.identity_receiver.borrow_and_update().clone();
                    if let Some(new_identity) = new_identity {
                        self.apply_identity_change(new_identity);
                    }
                }
                // The peer set changed: reconcile the connection table right away.
                changed = peer_list_receiver.changed() => {
                    if changed.is_err() {
                        debug_assert!(
                            self.exit_signals.is_exiting(),
                            "OutboundLoop: peer_list closed while running."
                        );
                        break;
                    }
                    self.reconcile();
                }
                Some(joined) = self.in_flight_handshakes.join_next(), if !self.in_flight_handshakes.is_empty() => {
                    match joined {
                        Ok(outcome) => self.handle_handshake_outcome(outcome),
                        Err(err) => error!("Outbound connection task failed: {err}"),
                    }
                }
                maybe_message = self.egress_receiver.recv() => {
                    let Some(message) = maybe_message else {
                        // ok here since the only way this happens is if Endpoint is already dropped.
                        break;
                    };
                    self.perform_broadcast(message);
                }
                // Periodic reconcile: connect to missing peers, drop departed peers.
                _ = reconcile_timer.tick() => self.reconcile(),
                _ = metrics.tick() => {
                    debug!("OutboundLoop: running bookkeeping tasks");
                    stats::report_client(&self.stats, self.peer_state.len() as u64);
                }
                _ = self.exit_signals.cancelled() => break,
            }
        }
    }

    /// Rebuild the client TLS config against the new identity, swap it into the
    /// quinn endpoint, close every existing connection (since they use old ID),
    /// and adopt the new pubkey. The next reconcile will reconnect to everyone.
    fn apply_identity_change(&mut self, new_identity: Arc<Identity>) {
        let client_config = new_client_config(
            new_identity.cert.clone(),
            new_identity.key.clone_key(),
            ALPENGLOW_ALPN,
        );
        self.local_pubkey = new_identity.pubkey;
        self.endpoint.set_default_client_config(client_config);
        // Dropping the JoinSet aborts every in-flight handshake. We do not
        // want them as they began under the old identity.
        self.in_flight_handshakes = JoinSet::new();
        let closed = self
            .peer_state
            .drain()
            .filter(|(_peer, entry)| {
                if let PeerState::Established { connection, .. } = entry {
                    close_codes::IDENTITY_CHANGED.close(connection);
                    true
                } else {
                    false
                }
            })
            .count() as u64;
        self.stats
            .connection_closed_identity_changed
            .fetch_add(closed, Ordering::Relaxed);
        info!(
            "outbound identity changed to {} ({} connection(s) closed)",
            new_identity.pubkey, closed
        );
    }

    /// Reconcile the connection table against the current peer_list.
    fn reconcile(&mut self) {
        debug!("OutboundLoop: running reconcile");
        // Clone the Arc so the operation is coherent (next call to reconcile will
        // pick up a new peer_list if it gets published before this operation finishes).
        let peer_list = self.peer_list_receiver.borrow().clone();

        // 1. Close connections for peers that have left the peer_list.
        let mut closed_not_in_peer_list = 0u64;
        self.peer_state.retain(|peer, state| {
            if peer_list.contains_key(peer) {
                return true;
            }
            // Only kill established connections, let handshakes resolve, they will
            // get reclaimed on the next iterations.
            match state {
                PeerState::Established { connection, .. } => {
                    info!("OutboundLoop: closing connection to {peer}: no longer desired.");
                    close_codes::NOT_ADMITTED.close(connection);
                    closed_not_in_peer_list = closed_not_in_peer_list.saturating_add(1);
                    false
                }
                _ => true,
            }
        });
        self.stats
            .connection_closed_not_in_peer_list
            .fetch_add(closed_not_in_peer_list, Ordering::Relaxed);

        // 2. Ensure a connection for every addressable peer.
        for (peer, addr) in peer_list.iter() {
            if *peer == self.local_pubkey {
                continue;
            }
            // No gossip address yet (or anymore)
            if addr.ip().is_unspecified() {
                self.stats
                    .connect_failed_no_address
                    .fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let needs_connection = match self.peer_state.get(peer) {
                Some(PeerState::Connecting) => false,
                Some(PeerState::Established {
                    connection,
                    target_address: cur,
                }) => {
                    // desired peer address has changed
                    if cur != addr {
                        info!(
                            "OutboundLoop: closing connection to {peer}: desired address changed \
                             from {cur} to {addr}."
                        );
                        close_codes::PEER_MOVED.close(connection);
                        self.stats
                            .connection_closed_peer_moved
                            .fetch_add(1, Ordering::Relaxed);
                        true
                    } else {
                        // Reconnect if the connection has died.
                        connection
                            .close_reason()
                            .inspect(|reason| {
                                self.stats.connection_lost.fetch_add(1, Ordering::Relaxed);
                                info!("OutboundLoop: connection to {peer} was closed: {reason}");
                            })
                            .is_some()
                    }
                }
                None => true,
            };
            if needs_connection {
                info!("OutboundLoop: initiating connection to {peer} ({addr})");
                self.in_flight_handshakes.spawn(connect(
                    self.endpoint.clone(),
                    *peer,
                    *addr,
                    self.stats.clone(),
                ));
                self.peer_state.insert(*peer, PeerState::Connecting);
            }
        }
    }

    /// Broadcast one message to every live connection. Broken connections are
    /// dropped from the table so the next reconcile remakes them.
    fn perform_broadcast(&mut self, message: Bytes) {
        let mut sent = 0u64;
        let mut dead_peers = Vec::new();
        for (peer, state) in self.peer_state.iter() {
            let PeerState::Established { connection, .. } = state else {
                continue;
            };
            match connection.send_datagram(message.clone()) {
                Ok(()) => sent = sent.saturating_add(1),
                Err(SendDatagramError::ConnectionLost(_)) => {
                    self.stats.connection_lost.fetch_add(1, Ordering::Relaxed);
                    dead_peers.push(*peer);
                }
                Err(e) => record_client_error(&Error::from(e), &self.stats),
            }
        }
        self.stats.datagrams_sent.fetch_add(sent, Ordering::Relaxed);
        for peer in dead_peers {
            self.peer_state.remove(&peer);
        }
    }

    fn handle_handshake_outcome(&mut self, outcome: HandshakeOutcome) {
        match outcome {
            Ok((peer, connection)) => match self.peer_state.get_mut(&peer) {
                Some(slot @ PeerState::Connecting) => {
                    *slot = PeerState::Established {
                        target_address: connection.remote_address(),
                        connection,
                    };
                    info!("OutboundLoop: established connection to {peer}.");
                    stats::record_connection_count(
                        &self.stats.peak_connections,
                        self.peer_state.len() as u64,
                    );
                }
                _ => debug_assert!(
                    false,
                    "Handshake completed for peer {peer} whose state was not Connecting."
                ),
            },
            // The slot is always `Connecting` here (same invariant as the Ok arm):
            // one handshake per peer, reconcile keeps `Connecting`, and identity
            // rotation resets the JoinSet, so no stale or duplicate outcome can
            // reach us. Drop the placeholder so the next reconcile retries.
            Err(peer) => {
                let removed = self.peer_state.remove(&peer);
                debug_assert!(
                    matches!(removed, Some(PeerState::Connecting)),
                    "Handshake failed for {peer} whose state was not Connecting."
                );
            }
        }
    }
}
