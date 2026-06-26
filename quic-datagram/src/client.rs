//! Outbound (client) direction: we-dial, send-only.
use {
    crate::{
        ALPENGLOW_ALPN, CONN_EVENT_CHANNEL_CAP, MAX_ALPENGLOW_VOTE_ACCOUNTS, METRICS_INTERVAL,
        close_codes,
        endpoint::Datagram,
        error::Error,
        stats::{self, QuicDatagramStats, add, record_error},
        transport::{IdentitySnapshot, new_client_config},
    },
    bytes::Bytes,
    log::{error, info, warn},
    lru::LruCache,
    quinn::{Connection, Endpoint, SendDatagramError},
    solana_net_utils::banlist::Banlist,
    solana_pubkey::{Pubkey, PubkeyHasherBuilder},
    solana_tls_utils::{get_remote_pubkey, socket_addr_to_quic_server_name},
    std::{
        mem,
        net::SocketAddr,
        num::NonZeroUsize,
        sync::{Arc, atomic::Ordering},
    },
    tokio::{
        spawn,
        sync::{mpsc, watch},
        time::{MissedTickBehavior, interval},
    },
    tokio_util::sync::CancellationToken,
};

/// State of a peer's entry in the outbound table.
pub(crate) enum PeerState {
    /// Connecting attempt in progress; holds the most recent datagram to send once
    /// the connection is up (newer arrivals overwrite older ones).
    Connecting { initial_buffer: Bytes },
    /// Live send-only connection. `addr` is cached at install (no migration on
    /// our send-only client) so the egress hot path avoids a quinn state-lock
    /// acquisition for `remote_address()` on every datagram.
    Established {
        connection: Connection,
        addr: SocketAddr,
    },
}

/// Dial result reported by a dial task to the outbound control loop.
pub(crate) struct ConnectEvent {
    pub(crate) peer: Pubkey,
    pub(crate) generation: u64,
    pub(crate) outcome: Result<Connection, ()>,
}

pub(crate) struct ClientConnection {
    pub(crate) endpoint: Endpoint,
    pub(crate) peer: Pubkey,
    pub(crate) addr: SocketAddr,
    pub(crate) generation: u64,
    pub(crate) events: mpsc::Sender<ConnectEvent>,
    pub(crate) stats: Arc<QuicDatagramStats>,
}

impl ClientConnection {
    async fn run(self) {
        let outcome = match self.dial().await {
            Ok(connection) => Ok(connection),
            Err(e) => {
                error!(
                    "Connection attempt to ({}, {}) failed: {e:?}",
                    self.peer, self.addr
                );
                record_error(&e, &self.stats);
                Err(())
            }
        };
        // Blocking send to make sure we clear the `Connecting` placeholder. If the
        // send fails there is nothing left to do.
        let _ = self
            .events
            .send(ConnectEvent {
                peer: self.peer,
                generation: self.generation,
                outcome,
            })
            .await;
    }

    async fn dial(&self) -> Result<Connection, Error> {
        let server_name = socket_addr_to_quic_server_name(self.addr);
        let connection = self.endpoint.connect(self.addr, &server_name)?.await?;
        let attested = get_remote_pubkey(&connection).ok_or(Error::InvalidIdentity(self.addr))?;
        if attested != self.peer {
            close_codes::INVALID_IDENTITY.close(&connection);
            return Err(Error::InvalidIdentity(self.addr));
        }
        Ok(connection)
    }
}

/// Outbound control loop: client egress (we-dial, send-only). Owns the per-peer
/// send-only connection state and the dial-task event channel.
pub(crate) struct OutboundLoop {
    pub(crate) endpoint: Endpoint,
    pub(crate) local_pubkey: Pubkey,
    /// Identity-rotation counter, local to this loop.
    pub(crate) generation: u64,
    pub(crate) egress_rx: mpsc::Receiver<Datagram>,
    pub(crate) banlist: Arc<Banlist<Pubkey>>,
    pub(crate) identity_rx: watch::Receiver<Option<Arc<IdentitySnapshot>>>,
    /// Per-peer send-only connection state.
    /// Idle connections are reclaimed lazily: `peer_state` is the sole owner of
    /// each `Connection`, so anything evicted/popped from the LRU closes the
    /// connection via quinn's implicit close.
    pub(crate) peer_state: LruCache<Pubkey, PeerState, PubkeyHasherBuilder>,
    /// Channel for spawned tasks to report their lifetime events
    pub(crate) events_tx: mpsc::Sender<ConnectEvent>,
    /// Channel for spawned tasks to report their lifetime events
    pub(crate) events_rx: mpsc::Receiver<ConnectEvent>,
    pub(crate) shutdown: CancellationToken,
    pub(crate) stats: Arc<QuicDatagramStats>,
}

impl OutboundLoop {
    /// Capacity of the peer-state table. The LRU owns every send-only
    /// `Connection`. It is sized to the full expected peer set (one outbound
    /// per peer) with headroom, old connections are recycled as needed.
    const LRU_SIZE: NonZeroUsize =
        NonZeroUsize::new(2 * MAX_ALPENGLOW_VOTE_ACCOUNTS).expect("LRU size is non-zero");

    pub(crate) fn new(
        endpoint: Endpoint,
        local_pubkey: Pubkey,
        egress_rx: mpsc::Receiver<Datagram>,
        banlist: Arc<Banlist<Pubkey>>,
        identity_rx: watch::Receiver<Option<Arc<IdentitySnapshot>>>,
        shutdown: CancellationToken,
        stats: Arc<QuicDatagramStats>,
    ) -> Self {
        let (events_tx, events_rx) = mpsc::channel::<ConnectEvent>(CONN_EVENT_CHANNEL_CAP);
        Self {
            endpoint,
            local_pubkey,
            generation: 0,
            egress_rx,
            banlist,
            identity_rx,
            peer_state: LruCache::with_hasher(Self::LRU_SIZE, PubkeyHasherBuilder::default()),
            events_tx,
            events_rx,
            shutdown,
            stats,
        }
    }

    pub(crate) async fn run(mut self) {
        let mut bookkeeping_timer = interval(METRICS_INTERVAL);
        bookkeeping_timer.set_missed_tick_behavior(MissedTickBehavior::Skip);

        // The identity arm tolerates the `KeyUpdater` sender being dropped (some
        // local-cluster tests drop it); once closed we stop polling that arm.
        let mut id_closed = false;
        loop {
            tokio::select! {
                biased;
                // ID changes are rare but very important
                changed = self.identity_rx.changed(), if !id_closed => {
                    if changed.is_err() {
                        warn!("identity rotation channel closed; outbound loop running without rotation support");
                        id_closed = true;
                        continue;
                    }
                    let snap = self.identity_rx.borrow_and_update().clone();
                    if let Some(snap) = snap {
                        self.apply_identity_change(snap);
                    }
                }
                // Dial outcomes must come ahead of egress to ensure new
                // connections are registered even when egress is busy.
                Some(event) = self.events_rx.recv() => self.handle_dial_event(event),
                // Egress
                maybe_datagram = self.egress_rx.recv() => {
                    let Some(datagram) = maybe_datagram else { break };
                    self.handle_datagram(datagram);
                }
                // Bookkeeping is best effort
                _ = bookkeeping_timer.tick() => {
                    stats::report_client(&self.stats, self.peer_state.len() as u64);
                }
                // Shutdown is never something we do in a hurry
                _ = self.shutdown.cancelled() => break,
            }
        }
    }

    /// Rebuild the client TLS config against the new identity, swap it into the
    /// quinn endpoint, evict the peer-state table so peers are re-dialed, and
    /// adopt the new pubkey.
    fn apply_identity_change(&mut self, snap: Arc<IdentitySnapshot>) {
        let client_config =
            new_client_config(snap.cert.clone(), snap.key.clone_key(), ALPENGLOW_ALPN);
        self.local_pubkey = snap.pubkey;
        self.endpoint.set_default_client_config(client_config);
        // Bump first so any in-flight dial that completes after this point is
        // dropped at the event boundary (its event carries the old generation).
        self.generation = self.generation.wrapping_add(1);
        let evicted = self
            .peer_state
            .iter()
            .map(|(_peer, entry)| {
                if let PeerState::Established { connection, .. } = entry {
                    close_codes::IDENTITY_ROTATED.close(connection);
                    1
                } else {
                    0
                }
            })
            .sum();
        self.peer_state.clear();
        self.stats
            .connection_evicted_identity_rotated
            .fetch_add(evicted, Ordering::Relaxed);
        info!(
            "outbound identity rotated to {} ({} connection(s) evicted)",
            snap.pubkey, evicted
        );
    }

    fn handle_datagram(&mut self, datagram: Datagram) {
        let Datagram {
            peer_pubkey: peer,
            peer_address: addr,
            message: bytes,
        } = datagram;
        debug_assert_ne!(self.local_pubkey, peer, "egress to self is a caller bug");
        if self.banlist.is_banned(&peer) {
            return;
        }

        if let Some(entry) = self.peer_state.get_mut(&peer) {
            match entry {
                PeerState::Connecting { initial_buffer } => {
                    // Newer datagrams replace older ones.
                    *initial_buffer = bytes;
                    self.stats
                        .egress_dropped_dial_in_progress
                        .fetch_add(1, Ordering::Relaxed);
                    return;
                }
                PeerState::Established {
                    connection,
                    addr: conn_addr,
                } if *conn_addr == addr => {
                    match connection.send_datagram(bytes.clone()) {
                        Ok(()) => {
                            add(&self.stats.datagrams_sent);
                            return;
                        }
                        Err(SendDatagramError::ConnectionLost(_)) => {
                            // Connection is dead; swap to Connecting.
                            *entry = PeerState::Connecting {
                                initial_buffer: bytes,
                            };
                        }
                        Err(e) => {
                            record_error(&Error::from(e), &self.stats);
                            return;
                        }
                    }
                }
                PeerState::Established { .. } => {
                    // Peer moved - swap the slot to `Connecting`...
                    let old = mem::replace(
                        entry,
                        PeerState::Connecting {
                            initial_buffer: bytes,
                        },
                    );
                    // ... and close the displaced connection with PEER_MOVED.
                    if let PeerState::Established {
                        connection: old_connection,
                        ..
                    } = old
                    {
                        close_codes::PEER_MOVED.close(&old_connection);
                        self.stats
                            .connection_evicted_peer_moved
                            .fetch_add(1, Ordering::Relaxed);
                        info!("peer {peer} moved; re-dialing at {addr}");
                    }
                }
            }
        } else {
            // Register a dialing placeholder and proceed to dial.
            // If this overflows the LRU it evicts the least-recently-used entry;
            // any displaced `Connection` is closed when it is dropped.
            // Any connection idle long enough to fall out of the LRU should
            // already have been axed, so there is nothing useful left to signal.
            self.peer_state.push(
                peer,
                PeerState::Connecting {
                    initial_buffer: bytes,
                },
            );
        };

        spawn(
            ClientConnection {
                endpoint: self.endpoint.clone(),
                peer,
                addr,
                generation: self.generation,
                events: self.events_tx.clone(),
                stats: self.stats.clone(),
            }
            .run(),
        );
    }

    /// Cleans up Connecting placeholder (if still there), then registers the connection.
    fn handle_dial_event(&mut self, event: ConnectEvent) {
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
                Some(slot @ PeerState::Connecting { .. }) => {
                    // Extract the latest initial_buffer and install the live connection.
                    let PeerState::Connecting { initial_buffer } = mem::replace(
                        slot,
                        PeerState::Established {
                            addr: connection.remote_address(),
                            connection: connection.clone(),
                        },
                    ) else {
                        unreachable!()
                    };
                    match connection.send_datagram(initial_buffer) {
                        Ok(()) => add(&self.stats.datagrams_sent),
                        Err(e) => record_error(&Error::from(e), &self.stats),
                    }
                    self.stats
                        .record_connection_count(self.peer_state.len() as u64);
                }
                _ => {
                    // Dial succeeded but the slot is no longer waiting for it
                    // (peer LRU-evicted mid-dial, or a newer dial already
                    // installed a connection). The connection is redundant;
                    // close it. This is vanishingly rare and not worth a metric.
                    close_codes::IDENTITY_ROTATED.close(&connection);
                }
            },
            Err(()) => {
                if matches!(
                    self.peer_state.peek(&peer),
                    Some(PeerState::Connecting { .. })
                ) {
                    self.peer_state.pop(&peer);
                }
            }
        }
    }
}
