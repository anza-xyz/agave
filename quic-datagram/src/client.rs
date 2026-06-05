//! Outbound (client) direction: we-dial, send-only.
use {
    crate::{
        ALPENGLOW_ALPN, EGRESS_IDLE_REAP_THRESHOLD, close_codes,
        endpoint::{Datagram, METRICS_INTERVAL},
        error::Error,
        stats::{self, QuicDatagramStats, add, record_error},
        transport::{IdentitySnapshot, new_client_config},
    },
    bytes::Bytes,
    log::{error, info, warn},
    quinn::{Connection, Endpoint, SendDatagramError},
    solana_net_utils::banlist::Banlist,
    solana_pubkey::{Pubkey, PubkeyHasherBuilder},
    solana_tls_utils::{get_remote_pubkey, socket_addr_to_quic_server_name},
    std::{
        collections::{HashMap, hash_map::Entry},
        mem,
        net::SocketAddr,
        sync::{Arc, atomic::Ordering},
    },
    tokio::{
        spawn,
        sync::{mpsc, watch},
        time::{Instant, MissedTickBehavior, interval},
    },
    tokio_util::sync::CancellationToken,
};

/// State of a peer's entry in the outbound table.
pub(crate) enum OutgoingEntry {
    /// Dialing attempt in progress.
    Dialing,
    Established {
        conn: Connection,
        /// When we last sent a datagram on the connection
        last_used: Instant,
    },
}

/// Event reported by a dial task or close-watcher to the outbound control loop.
pub(crate) enum OutboundEvent {
    /// Dial result: `Ok(conn)` on success, `Err(())` if the dial or identity
    /// check failed.
    Dialed {
        peer: Pubkey,
        generation: u64,
        outcome: Result<Connection, ()>,
    },
    /// An established outbound connection closed; the loop reaps its slot.
    Closed {
        peer: Pubkey,
        generation: u64,
        stable_id: usize,
    },
}

pub(crate) struct ClientConnection {
    pub(crate) endpoint: Endpoint,
    pub(crate) peer: Pubkey,
    pub(crate) addr: SocketAddr,
    pub(crate) generation: u64,
    pub(crate) trigger: Bytes,
    pub(crate) events: mpsc::Sender<OutboundEvent>,
    pub(crate) stats: Arc<QuicDatagramStats>,
}

impl ClientConnection {
    async fn run(self) {
        let outcome = match self.dial().await {
            Ok(conn) => {
                match conn.send_datagram(self.trigger) {
                    Ok(()) => add(&self.stats.datagrams_sent),
                    Err(e) => record_error(&Error::from(e), &self.stats),
                }
                Ok(conn)
            }
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
            .send(OutboundEvent::Dialed {
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
    pub(crate) outgoing: HashMap<Pubkey, OutgoingEntry, PubkeyHasherBuilder>,
    /// Channel for spawned tasks to report their lifetime events
    pub(crate) events_tx: mpsc::Sender<OutboundEvent>,
    /// Channel for spawned tasks to report their lifetime events
    pub(crate) events_rx: mpsc::Receiver<OutboundEvent>,
    pub(crate) shutdown: CancellationToken,
    pub(crate) stats: Arc<QuicDatagramStats>,
}

impl OutboundLoop {
    /// Outbound packet dispatch. Returns `true` if the
    /// caller must spawn a dial task.
    ///
    /// Internally tihs runs the 4-case decision:
    /// * vacant -> initiate new connection (-> true),
    /// * dial in progress -> drop (-> false),
    /// * established to `addr`
    ///     * send success (-> false),
    ///     * send failed due to dead connection (-> true),
    /// * established to a different addr -> evict (`PEER_MOVED`) (-> true).
    fn send_outbound(&mut self, peer: Pubkey, addr: SocketAddr, bytes: &Bytes) -> bool {
        match self.outgoing.entry(peer) {
            Entry::Vacant(slot) => {
                slot.insert(OutgoingEntry::Dialing);
                true
            }
            Entry::Occupied(mut slot) => match slot.get() {
                OutgoingEntry::Dialing => {
                    self.stats
                        .egress_dropped_dial_in_progress
                        .fetch_add(1, Ordering::Relaxed);
                    false
                }
                OutgoingEntry::Established { conn, .. } if conn.remote_address() == addr => {
                    match conn.send_datagram(bytes.clone()) {
                        Ok(()) => {
                            add(&self.stats.datagrams_sent);
                            // Mark the connection as freshly used so the idle
                            // reaper leaves it alone.
                            if let OutgoingEntry::Established { last_used, .. } = slot.get_mut() {
                                *last_used = Instant::now();
                            }
                            false
                        }
                        Err(SendDatagramError::ConnectionLost(_)) => {
                            // Connection is dead; swap to Dialing so the
                            // caller re-dials with this datagram as trigger.
                            *slot.get_mut() = OutgoingEntry::Dialing;
                            true
                        }
                        Err(e) => {
                            record_error(&Error::from(e), &self.stats);
                            false
                        }
                    }
                }
                OutgoingEntry::Established { .. } => {
                    // Peer moved - swap the slot to `Dialing`...
                    let old = mem::replace(slot.get_mut(), OutgoingEntry::Dialing);
                    // ... and close the displaced connection with PEER_MOVED.
                    if let OutgoingEntry::Established { conn: old_conn, .. } = old {
                        close_codes::PEER_MOVED.close(&old_conn);
                        self.stats
                            .connection_evicted_peer_moved
                            .fetch_add(1, Ordering::Relaxed);
                        info!("peer {peer} moved; re-dialing at {addr}");
                    }
                    true
                }
            },
        }
    }

    /// Close established outbound connections that upstream is not using.
    fn close_idle_connections(&mut self) {
        let now = Instant::now();
        let mut reaped = 0u64;
        self.outgoing.retain(|_peer, entry| {
            if let OutgoingEntry::Established { conn, last_used } = entry
                && now.duration_since(*last_used) >= EGRESS_IDLE_REAP_THRESHOLD
            {
                close_codes::IDLE.close(conn);
                reaped = reaped.saturating_add(1);
                return false;
            }
            true
        });
        if reaped > 0 {
            self.stats
                .connection_evicted_idle
                .fetch_add(reaped, Ordering::Relaxed);
            info!("Closed {reaped} idle outbound connection(s)");
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
                Some(event) = self.events_rx.recv() => self.handle_event(event),
                // Egress
                maybe_datagram = self.egress_rx.recv() => {
                    let Some(datagram) = maybe_datagram else { break };
                    self.handle_datagram(datagram);
                }
                // Bookkeeping is best effort
                _ = bookkeeping_timer.tick() => {
                    self.close_idle_connections();
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
            .drain()
            .map(|(_, entry)| {
                if let OutgoingEntry::Established { conn, .. } = entry {
                    close_codes::IDENTITY_ROTATED.close(&conn);
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

        if !self.send_outbound(peer, addr, &bytes) {
            return;
        }

        spawn(
            ClientConnection {
                endpoint: self.endpoint.clone(),
                peer,
                addr,
                generation: self.generation,
                trigger: bytes,
                events: self.events_tx.clone(),
                stats: self.stats.clone(),
            }
            .run(),
        );
    }

    /// Apply a dial result or a connection-close notification.
    fn handle_event(&mut self, event: OutboundEvent) {
        let (peer, generation) = match &event {
            OutboundEvent::Dialed {
                peer, generation, ..
            }
            | OutboundEvent::Closed {
                peer, generation, ..
            } => (*peer, *generation),
        };
        if generation != self.generation {
            if let OutboundEvent::Dialed {
                outcome: Ok(conn), ..
            } = event
            {
                close_codes::IDENTITY_ROTATED.close(&conn);
                self.stats
                    .connection_evicted_identity_rotated
                    .fetch_add(1, Ordering::Relaxed);
            }
            return;
        }
        match event {
            OutboundEvent::Dialed {
                outcome: Ok(conn), ..
            } => match self.outgoing.get_mut(&peer) {
                Some(slot @ OutgoingEntry::Dialing) => {
                    // The dial task already sent the trigger datagram, so the
                    // connection is freshly used as of now.
                    *slot = OutgoingEntry::Established {
                        conn: conn.clone(),
                        last_used: Instant::now(),
                    };
                    self.stats
                        .record_connection_count(self.outgoing.len() as u64);
                    self.spawn_close_watcher(peer, conn);
                }
                _ => {
                    close_codes::IDENTITY_ROTATED.close(&conn);
                    record_error(&Error::IdentityRotated(peer), &self.stats);
                }
            },
            OutboundEvent::Dialed {
                outcome: Err(()), ..
            } => {
                if let Entry::Occupied(slot) = self.outgoing.entry(peer)
                    && matches!(slot.get(), OutgoingEntry::Dialing)
                {
                    slot.remove();
                }
            }
            // Reap only if the closed connection is still the one we hold; a
            // re-dial may have already replaced it.
            OutboundEvent::Closed { stable_id, .. } => {
                if let Entry::Occupied(slot) = self.outgoing.entry(peer)
                    && matches!(slot.get(), OutgoingEntry::Established { conn, .. } if conn.stable_id() == stable_id)
                {
                    slot.remove();
                }
            }
        }
    }

    /// Watch an established connection and report [`OutboundEvent::Closed`] when
    /// it ends so the loop can reap the slot.
    fn spawn_close_watcher(&self, peer: Pubkey, conn: Connection) {
        let events = self.events_tx.clone();
        let generation = self.generation;
        spawn(async move {
            let stable_id = conn.stable_id();
            conn.closed().await;
            let _ = events
                .send(OutboundEvent::Closed {
                    peer,
                    generation,
                    stable_id,
                })
                .await;
        });
    }
}
