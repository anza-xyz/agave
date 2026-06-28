//! Outbound (client) direction: we initiate, send-only.
use {
    crate::{
        ALPENGLOW_ALPN, CONN_EVENT_CHANNEL_CAP, METRICS_INTERVAL, PeerListReceiver, close_codes,
        error::Error,
        stats::{self, ClientStats, add, record_client_error},
        transport::{IdentitySnapshot, new_client_config},
    },
    bytes::Bytes,
    log::{error, info},
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
        spawn,
        sync::{mpsc, watch},
        time::{MissedTickBehavior, interval},
    },
    tokio_util::sync::CancellationToken,
};

/// How often the outbound loop reconciles its connection table against the
/// peer_list. Doubles as the retry interval for failed connects.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(1);

/// State of a peer's entry in the outbound table.
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

/// Connect result reported by a ClientConnection task to the OubtoundLoop.
pub(crate) struct ConnectEvent {
    pub(crate) peer: Pubkey,
    pub(crate) generation: u64,
    pub(crate) outcome: Result<Connection, ()>,
}

/// Task for the outbound connection.
pub(crate) struct ClientConnection {
    pub(crate) endpoint: Endpoint,
    pub(crate) peer: Pubkey,
    pub(crate) addr: SocketAddr,
    pub(crate) generation: u64,
    pub(crate) events_sender: mpsc::Sender<ConnectEvent>,
    pub(crate) stats: Arc<ClientStats>,
}

impl ClientConnection {
    async fn run(self) {
        let connect = async {
            let server_name = socket_addr_to_quic_server_name(self.addr);
            let connection = self.endpoint.connect(self.addr, &server_name)?.await?;
            let attested =
                get_remote_pubkey(&connection).ok_or(Error::InvalidIdentity(self.addr))?;
            if attested != self.peer {
                close_codes::INVALID_IDENTITY.close(&connection);
                return Err(Error::InvalidIdentity(self.addr));
            }
            Ok(connection)
        };
        let outcome = match connect.await {
            Ok(connection) => Ok(connection),
            Err(e) => {
                error!(
                    "Connection attempt to ({}, {}) failed: {e:?}",
                    self.peer, self.addr
                );
                record_client_error(&e, &self.stats);
                Err(())
            }
        };
        // Report back to the OutboundLoop. If send fails
        // the OutboundLoop must have exited.
        let _ = self
            .events_sender
            .send(ConnectEvent {
                peer: self.peer,
                generation: self.generation,
                outcome,
            })
            .await;
    }
}

/// Outbound control loop for the egress direction(we-connect, send-only).
/// Strives to ensure open connections to everyone in the peer_list.
pub(crate) struct OutboundLoop {
    pub(crate) endpoint: Endpoint,
    pub(crate) local_pubkey: Pubkey,
    /// Identity-rotation counter.
    pub(crate) generation: u64,
    /// Channel for outbound messages to be broadcast
    pub(crate) egress_receiver: mpsc::Receiver<Bytes>,
    pub(crate) identity_receiver: watch::Receiver<Option<Arc<IdentitySnapshot>>>,
    pub(crate) peer_list_receiver: PeerListReceiver,
    /// Per-peer send-only connection state.
    /// Size is limited to the peer_list size by reconcile task.
    pub(crate) peer_state: HashMap<Pubkey, PeerState, PubkeyHasherBuilder>,
    /// Sender to clone into freshly spawned per-connection tasks
    pub(crate) events_sender: mpsc::Sender<ConnectEvent>,
    /// Per-connection tasks report their outcome here
    pub(crate) events_receiver: mpsc::Receiver<ConnectEvent>,
    pub(crate) shutdown: CancellationToken,
    pub(crate) stats: Arc<ClientStats>,
}

impl OutboundLoop {
    pub(crate) fn new(
        endpoint: Endpoint,
        local_pubkey: Pubkey,
        egress_receiver: mpsc::Receiver<Bytes>,
        identity_receiver: watch::Receiver<Option<Arc<IdentitySnapshot>>>,
        peer_list_receiver: PeerListReceiver,
        shutdown: CancellationToken,
    ) -> Self {
        let (events_sender, events_receiver) =
            mpsc::channel::<ConnectEvent>(CONN_EVENT_CHANNEL_CAP);
        Self {
            endpoint,
            local_pubkey,
            generation: 0,
            egress_receiver,
            identity_receiver,
            peer_list_receiver,
            peer_state: HashMap::with_hasher(PubkeyHasherBuilder::default()),
            events_sender,
            events_receiver,
            shutdown,
            stats: Arc::default(),
        }
    }

    pub(crate) async fn run(mut self) {
        let mut metrics = interval(METRICS_INTERVAL);
        metrics.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let mut reconcile_timer = interval(RECONCILE_INTERVAL);
        reconcile_timer.set_missed_tick_behavior(MissedTickBehavior::Delay);

        // A separate receiver clone drives change notifications, the snapshot
        // reads in `reconcile` go through `self.peer_list_receiver`.
        let mut peer_list_receiver = self.peer_list_receiver.clone();

        loop {
            tokio::select! {
                biased;
                // ID changes are rare but very important and must be acted on immediately
                changed = self.identity_receiver.changed() => {
                    if changed.is_err(){
                        info!("identity rotation channel closed; outbound loop exiting");
                        break;
                    }
                    let snap = self.identity_receiver.borrow_and_update().clone();
                    if let Some(snap) = snap {
                        self.apply_identity_change(snap);
                    }
                }
                // The peer set changed: reconcile the connection table right away.
                changed = peer_list_receiver.changed() => {
                    if changed.is_err() {
                        info!("peer_list channel closed; outbound loop exiting");
                        break;
                    }
                    self.reconcile();
                }
                // Connect outcomes must come ahead of egress to ensure new
                // connections are registered even when egress is busy.
                Some(event) = self.events_receiver.recv() => self.handle_connect_event(event),
                // Egress: broadcast one message to every live connection.
                maybe_message = self.egress_receiver.recv() => {
                    let Some(message) = maybe_message else { break };
                    self.handle_broadcast(message);
                }
                // Periodic reconcile: connect to missing peers, drop departed peers.
                _ = reconcile_timer.tick() => self.reconcile(),
                // Metrics are best effort
                _ = metrics.tick() => {
                    stats::report_client(&self.stats, self.peer_state.len() as u64);
                }
                // Shutdown is never something we do in a hurry
                _ = self.shutdown.cancelled() => break,
            }
        }
    }

    /// Rebuild the client TLS config against the new identity, swap it into the
    /// quinn endpoint, evict every existing connection (since they use old ID),
    /// and adopt the new pubkey. The next reconcile will reconnect to everyone.
    fn apply_identity_change(&mut self, snap: Arc<IdentitySnapshot>) {
        let client_config =
            new_client_config(snap.cert.clone(), snap.key.clone_key(), ALPENGLOW_ALPN);
        self.local_pubkey = snap.pubkey;
        self.endpoint.set_default_client_config(client_config);
        // Bump generation so any in-flight connect that completes after this point is
        // dropped (it carries the old ID).
        self.generation = self.generation.wrapping_add(1);
        let evicted = self
            .peer_state
            .drain()
            .filter(|(_peer, entry)| {
                if let PeerState::Established { connection, .. } = entry {
                    close_codes::IDENTITY_ROTATED.close(connection);
                    true
                } else {
                    false
                }
            })
            .count() as u64;
        self.stats
            .connection_evicted_identity_rotated
            .fetch_add(evicted, Ordering::Relaxed);
        info!(
            "outbound identity rotated to {} ({} connection(s) evicted)",
            snap.pubkey, evicted
        );
    }

    /// Reconcile the connection table against the current peer_list.
    fn reconcile(&mut self) {
        // Clone the Arc so the operation is coherent (next call to reconcile will
        // pick up a new peer_list if it gets published before this operation finishes).
        let snapshot = self.peer_list_receiver.borrow().clone();

        // 1. Drop connections for peers that left the peer_list.
        let mut evicted = 0u64;
        self.peer_state.retain(|peer, state| {
            if snapshot.contains_key(peer) {
                return true;
            }
            if let PeerState::Established { connection, .. } = state {
                close_codes::NOT_ADMITTED.close(connection);
                evicted = evicted.saturating_add(1);
            }
            false
        });
        self.stats
            .connection_evicted_peer_list
            .fetch_add(evicted, Ordering::Relaxed);

        // 2. Ensure a connection for every addressable peer.
        for (peer, addr) in snapshot.iter() {
            if *peer == self.local_pubkey {
                continue;
            }
            // No gossip address yet (or anymore)
            if addr.ip().is_unspecified() {
                add(&self.stats.connect_no_route);
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
                        close_codes::PEER_MOVED.close(connection);
                        self.stats
                            .connection_evicted_peer_moved
                            .fetch_add(1, Ordering::Relaxed);
                        true
                    } else {
                        // Reconnect if the connection has died.
                        connection.close_reason().is_some()
                    }
                }
                None => true,
            };
            if needs_connection {
                self.peer_state.insert(*peer, PeerState::Connecting);
                spawn(
                    ClientConnection {
                        endpoint: self.endpoint.clone(),
                        peer: *peer,
                        addr: *addr,
                        generation: self.generation,
                        events_sender: self.events_sender.clone(),
                        stats: self.stats.clone(),
                    }
                    .run(),
                );
            }
        }
    }

    /// Broadcast one message to every live connection. Broken connections are
    /// dropped from the table so the next reconcile remakes them.
    fn handle_broadcast(&mut self, message: Bytes) {
        let mut sent = 0u64;
        let mut dead_peers = Vec::new();
        for (peer, state) in self.peer_state.iter() {
            let PeerState::Established { connection, .. } = state else {
                continue;
            };
            match connection.send_datagram(message.clone()) {
                Ok(()) => sent = sent.saturating_add(1),
                Err(SendDatagramError::ConnectionLost(_)) => dead_peers.push(*peer),
                Err(e) => record_client_error(&Error::from(e), &self.stats),
            }
        }
        self.stats.datagrams_sent.fetch_add(sent, Ordering::Relaxed);
        for peer in dead_peers {
            self.peer_state.remove(&peer);
        }
    }

    /// React to a ConnectEvent.
    fn handle_connect_event(&mut self, event: ConnectEvent) {
        let ConnectEvent {
            peer,
            generation,
            outcome,
        } = event;
        if generation != self.generation {
            if let Ok(connection) = outcome {
                close_codes::IDENTITY_ROTATED.close(&connection);
                self.stats
                    .connection_evicted_identity_rotated
                    .fetch_add(1, Ordering::Relaxed);
            }
            return;
        }
        match outcome {
            Ok(connection) => match self.peer_state.get_mut(&peer) {
                Some(slot @ PeerState::Connecting) => {
                    *slot = PeerState::Established {
                        target_address: connection.remote_address(),
                        connection,
                    };
                    stats::record_connection_count(
                        &self.stats.peak_connections,
                        self.peer_state.len() as u64,
                    );
                }
                _ => {
                    // Connect succeeded but the slot is no longer waiting for it.
                    // The connection is redundant; close it.
                    close_codes::IDENTITY_ROTATED.close(&connection);
                }
            },
            // Connection failed: drop the placeholder.
            Err(()) => {
                debug_assert!(matches!(
                    self.peer_state.get(&peer),
                    Some(PeerState::Connecting)
                ));
                self.peer_state.remove(&peer);
            }
        }
    }
}
