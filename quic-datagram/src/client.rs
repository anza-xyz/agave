//! Outbound (client) direction: we-dial, send-only.
use {
    crate::{
        ALPENGLOW_ALPN, close_codes,
        endpoint::{Datagram, METRICS_INTERVAL},
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
pub(crate) enum OutgoingEntry {
    /// Dialing attempt in progress; holds the most recent datagram to send once
    /// the connection is up (newer arrivals overwrite older ones).
    Dialing { trigger: Bytes },
    /// Live send-only connection.
    Established { conn: Connection },
}

/// Dial result reported by a dial task to the outbound control loop.
pub(crate) struct DialEvent {
    pub(crate) peer: Pubkey,
    pub(crate) generation: u64,
    pub(crate) outcome: Result<Connection, ()>,
}

pub(crate) struct ClientConnection {
    pub(crate) endpoint: Endpoint,
    pub(crate) peer: Pubkey,
    pub(crate) addr: SocketAddr,
    pub(crate) generation: u64,
    pub(crate) events: mpsc::Sender<DialEvent>,
    pub(crate) stats: Arc<QuicDatagramStats>,
}

impl ClientConnection {
    async fn run(self) {
        let outcome = match self.dial().await {
            Ok(conn) => Ok(conn),
            Err(e) => {
                error!(
                    "Connection attempt to ({}, {}) failed: {e:?}",
                    self.peer, self.addr
                );
                record_error(&e, &self.stats);
                Err(())
            }
        };
        // Blocking send to make sure we clear the `Dialing` placeholder. If the
        // send fails there is nothing left to do.
        let _ = self
            .events
            .send(DialEvent {
                peer: self.peer,
                generation: self.generation,
                outcome,
            })
            .await;
    }

    async fn dial(&self) -> Result<Connection, Error> {
        let server_name = socket_addr_to_quic_server_name(self.addr);
        let connection = self.endpoint.connect(self.addr, &server_name)?.await?;
        // Server identity must match the pubkey the caller targeted.
        let attested = get_remote_pubkey(&connection).ok_or(Error::InvalidIdentity(self.addr))?;
        if attested != self.peer {
            close_codes::INVALID_IDENTITY.close(&connection);
            return Err(Error::InvalidIdentity(self.addr));
        }
        Ok(connection)
    }
}

/// Outbound control loop: client egress (we-dial, send-only). Owns the outgoing
/// table and the dial-task event channel.
pub(crate) struct OutboundLoop {
    pub(crate) endpoint: Endpoint,
    pub(crate) local_pubkey: Pubkey,
    /// Identity-rotation counter, local to this loop.
    pub(crate) generation: u64,
    pub(crate) egress_rx: mpsc::Receiver<Datagram>,
    pub(crate) banlist: Arc<Banlist<Pubkey>>,
    pub(crate) identity_rx: watch::Receiver<Option<Arc<IdentitySnapshot>>>,
    /// Outbound table (per-peer send-only connection state).
    /// Idle connections are reclaimed lazily: `outgoing` is the sole owner of each
    /// `Connection`, so anything evicted/popped from the LRU closes the
    /// connection via quinn's implicit close.
    pub(crate) outgoing: LruCache<Pubkey, OutgoingEntry, PubkeyHasherBuilder>,
    /// Channel for spawned tasks to report their lifetime events
    pub(crate) events_tx: mpsc::Sender<DialEvent>,
    /// Channel for spawned tasks to report their lifetime events
    pub(crate) events_rx: mpsc::Receiver<DialEvent>,
    pub(crate) shutdown: CancellationToken,
    pub(crate) stats: Arc<QuicDatagramStats>,
}

impl OutboundLoop {
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
                    stats::report_client(&self.stats, self.outgoing.len() as u64);
                }
                // Shutdown is never something we do in a hurry
                _ = self.shutdown.cancelled() => break,
            }
        }
    }

    /// Rebuild the client TLS config against the new identity, swap it into the
    /// quinn endpoint, evict the outbound table so peers are re-dialed, and
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
            .outgoing
            .iter()
            .map(|(_peer, entry)| {
                if let OutgoingEntry::Established { conn } = entry {
                    close_codes::IDENTITY_ROTATED.close(conn);
                    1
                } else {
                    0
                }
            })
            .sum();
        self.outgoing.clear();
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

        if let Some(entry) = self.outgoing.get_mut(&peer) {
            match entry {
                OutgoingEntry::Dialing { trigger } => {
                    // Newer datagrams replace older ones.
                    *trigger = bytes;
                    self.stats
                        .egress_dropped_dial_in_progress
                        .fetch_add(1, Ordering::Relaxed);
                    return;
                }
                OutgoingEntry::Established { conn } if conn.remote_address() == addr => {
                    match conn.send_datagram(bytes.clone()) {
                        Ok(()) => {
                            add(&self.stats.datagrams_sent);
                            return;
                        }
                        Err(SendDatagramError::ConnectionLost(_)) => {
                            // Connection is dead; swap to Dialing.
                            *entry = OutgoingEntry::Dialing { trigger: bytes };
                        }
                        Err(e) => {
                            record_error(&Error::from(e), &self.stats);
                            return;
                        }
                    }
                }
                OutgoingEntry::Established { .. } => {
                    // Peer moved - swap the slot to `Dialing`...
                    let old = mem::replace(entry, OutgoingEntry::Dialing { trigger: bytes });
                    // ... and close the displaced connection with PEER_MOVED.
                    if let OutgoingEntry::Established { conn: old_conn } = old {
                        close_codes::PEER_MOVED.close(&old_conn);
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
            self.outgoing
                .push(peer, OutgoingEntry::Dialing { trigger: bytes });
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

    /// Cleans up Dialing placeholder (if still there), then registers the connection.
    fn handle_dial_event(&mut self, event: DialEvent) {
        let DialEvent {
            peer,
            generation,
            outcome,
        } = event;
        if generation != self.generation {
            if let Ok(conn) = outcome {
                close_codes::IDENTITY_ROTATED.close(&conn);
                self.stats
                    .connection_evicted_identity_rotated
                    .fetch_add(1, Ordering::Relaxed);
            }
            return;
        }
        match outcome {
            Ok(conn) => match self.outgoing.get_mut(&peer) {
                Some(slot @ OutgoingEntry::Dialing { .. }) => {
                    // Extract the latest trigger and install the live connection.
                    let OutgoingEntry::Dialing { trigger } =
                        mem::replace(slot, OutgoingEntry::Established { conn: conn.clone() })
                    else {
                        unreachable!()
                    };
                    match conn.send_datagram(trigger) {
                        Ok(()) => add(&self.stats.datagrams_sent),
                        Err(e) => record_error(&Error::from(e), &self.stats),
                    }
                    self.stats
                        .record_connection_count(self.outgoing.len() as u64);
                }
                _ => {
                    close_codes::IDENTITY_ROTATED.close(&conn);
                    record_error(&Error::IdentityRotated(peer), &self.stats);
                }
            },
            Err(()) => {
                if matches!(
                    self.outgoing.peek(&peer),
                    Some(OutgoingEntry::Dialing { .. })
                ) {
                    self.outgoing.pop(&peer);
                }
            }
        }
    }
}
