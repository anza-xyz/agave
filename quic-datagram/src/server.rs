//! Inbound (server) direction: we-accept, receive-only.

use {
    crate::{
        ALLOWLIST_CHECK_INTERVAL, ALPENGLOW_ALPN, BAN_DURATION_DOS,
        MAX_DATAGRAMS_PER_SECOND_PER_PEER, MAX_INBOUND_CONNECTIONS_PER_PEER, PEER_RATE_LIMIT_BURST,
        PEER_RATE_LIMIT_BURST_DOS,
        allowlist::Allowlist,
        close_codes,
        endpoint::{Datagram, METRICS_INTERVAL},
        error::Error,
        stats::{self, QuicDatagramStats, add, record_error},
        transport::{IdentitySnapshot, new_server_config},
    },
    arrayvec::ArrayVec,
    crossbeam_channel::{Sender, TrySendError},
    log::{debug, info, warn},
    quinn::{Connection, Endpoint, Incoming},
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
        time::{Instant, MissedTickBehavior, interval, sleep},
    },
    tokio_util::sync::CancellationToken,
};

/// Interval of pruning for banlist entries that are no longer valid.
const BANLIST_PRUNE_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Event reported by an accept or read task to the inbound control loop.
pub(crate) enum InboundEvent {
    /// A TLS handshake completed and yielded an authenticated peer.
    Accepted {
        peer: Pubkey,
        conn: Connection,
        generation: u64,
    },
    /// An inbound (we-accepted) connection ended. The read loop reports this
    /// so the control loop can reap the table slot.
    Closed {
        peer: Pubkey,
        generation: u64,
        stable_id: usize,
    },
}

/// An inbound accept: run the handshake and hand the connection (plus its
/// attested pubkey) to the control loop for admission.
pub(crate) struct ServerConnection {
    pub(crate) incoming: Incoming,
    pub(crate) generation: u64,
    pub(crate) events: mpsc::Sender<InboundEvent>,
    pub(crate) stats: Arc<QuicDatagramStats>,
}

impl ServerConnection {
    async fn run(self) {
        let remote_addr = self.incoming.remote_address();
        let conn = match async { self.incoming.accept()?.await }.await {
            Ok(conn) => conn,
            Err(e) => {
                record_error(&Error::from(e), &self.stats);
                return;
            }
        };
        let Some(peer) = get_remote_pubkey(&conn) else {
            close_codes::INVALID_IDENTITY.close(&conn);
            record_error(&Error::InvalidIdentity(remote_addr), &self.stats);
            return;
        };
        // Hand the connection to the loop for admission control checks
        let _ = self
            .events
            .send(InboundEvent::Accepted {
                peer,
                conn,
                generation: self.generation,
            })
            .await;
    }
}

/// Drive the per-connection read loop for an incoming connection.
/// Returns when the connection closes. On exit it reliably reports
/// [`InboundEvent::Closed`] so the control loop can reap the table entry.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn read_datagram_loop(
    connection: Connection,
    peer: Pubkey,
    remote_addr: SocketAddr,
    generation: u64,
    ingress: Sender<Datagram>,
    allowlist: Arc<dyn Allowlist>,
    banlist: Arc<Banlist<Pubkey>>,
    events: mpsc::Sender<InboundEvent>,
    stats: Arc<QuicDatagramStats>,
) {
    let stable_id = connection.stable_id();
    // Per-connection rate limiter. Any datagram arriving with the bucket
    // below RATE_LIMIT_WATERMARK is dropped, since honest peers
    // legitimately burst above the refill rate during catch-up.
    // Any packet arriving when bucket is empty is *closed*.
    const RATE_LIMIT_WATERMARK: u64 = PEER_RATE_LIMIT_BURST_DOS - PEER_RATE_LIMIT_BURST;
    let rate_limit = TokenBucket::new(
        PEER_RATE_LIMIT_BURST_DOS,
        PEER_RATE_LIMIT_BURST_DOS,
        MAX_DATAGRAMS_PER_SECOND_PER_PEER,
    );
    let mut allowlist_check = interval(ALLOWLIST_CHECK_INTERVAL);
    allowlist_check.tick().await; // skip the immediate first fire
    loop {
        tokio::select! {
            result = connection.read_datagram() => {
                match result {
                    Ok(bytes) => {
                        // Banlist check happens AFTER the read so a ban that
                        // lands while we're awaiting can't let a follow-up
                        // datagram leak through to ingress.
                        if banlist.is_banned(&peer) {
                            close_codes::BANNED.close(&connection);
                            break;
                        }
                        match rate_limit.consume_tokens(1) {
                            // green corridor - sender is behaving
                            Ok(remaining) if remaining > RATE_LIMIT_WATERMARK => {}
                            // red corridor - sender is bursting above normal
                            Ok(_) => {
                                drop(bytes);
                                stats.datagram_rate_limited.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                            // we are under attack - kick the sender.
                            Err(_) => {
                                drop(bytes);
                                banlist.ban(peer, BAN_DURATION_DOS);
                                close_codes::BANNED.close(&connection);
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
                        // The peer (or we) closed this inbound, or it timed
                        // out. Record and exit; the control loop reaps the
                        // table slot from the `Closed` event below.
                        record_error(&Error::from(e), &stats);
                        break;
                    }
                }
            }
            _ = allowlist_check.tick() => {
                if !allowlist.allow(&peer) {
                    close_codes::NOT_ADMITTED.close(&connection);
                    stats.connection_evicted_allowlist.fetch_add(1, Ordering::Relaxed);
                    break;
                }
            }
        }
    }
    // Send the notification to control that this connection died.
    let _ = events
        .send(InboundEvent::Closed {
            peer,
            generation,
            stable_id,
        })
        .await;
}

/// Inbound control loop: we-accept, receive-only.
pub(crate) struct InboundLoop {
    pub(crate) endpoint: Endpoint,
    /// Identity-rotation counter
    pub(crate) generation: u64,
    pub(crate) ingress: Sender<Datagram>,
    /// Policy to instantly ban all packets from a Pubkey
    pub(crate) banlist: Arc<Banlist<Pubkey>>,
    /// Policy for which peers may occupy a slot or retain a connection.
    pub(crate) allowlist: Arc<dyn Allowlist>,
    pub(crate) identity_rx: watch::Receiver<Option<Arc<IdentitySnapshot>>>,
    /// Inbound table (per-peer accepted receive-only connections), owned solely
    /// by this loop.
    pub(crate) incoming: HashMap<
        Pubkey,
        ArrayVec<Connection, MAX_INBOUND_CONNECTIONS_PER_PEER>,
        PubkeyHasherBuilder,
    >,
    /// Channel for read tasks to report their lifetime events.
    pub(crate) events_tx: mpsc::Sender<InboundEvent>,
    /// Channel for read tasks to report their lifetime events.
    pub(crate) events_rx: mpsc::Receiver<InboundEvent>,
    /// Global cap on inbound handshake rate across all source IPs.
    pub(crate) handshake_global_limiter: TokenBucket,
    pub(crate) stats: Arc<QuicDatagramStats>,
    pub(crate) shutdown: CancellationToken,
}

impl InboundLoop {
    /// Live inbound connections (each pubkey may hold several).
    fn incoming_len(&self) -> u64 {
        self.incoming.values().map(ArrayVec::len).sum::<usize>() as u64
    }

    /// Install a `connection` for `peer`. We keep up to
    /// [`MAX_INBOUND_CONNECTIONS_PER_PEER`] connections per pubkey.
    ///  Returns:
    /// - `Ok(())` if the connection took a slot (a fresh pubkey, or an
    ///   additional connection for a pubkey already present).
    /// - `Err(())` if this pubkey already holds the per-peer maximum.
    ///   This should be rare in normal operation.
    fn insert_inbound(&mut self, peer: Pubkey, connection: Connection) -> Result<(), ()> {
        match self.incoming.entry(peer) {
            Entry::Vacant(slot) => {
                let mut v = ArrayVec::new();
                v.push(connection);
                slot.insert(v);
                Ok(())
            }
            Entry::Occupied(mut slot) => slot.get_mut().try_push(connection).map_err(|_| ()),
        }
    }

    /// Remove the connection with the given `stable_id` from `peer`'s inbound
    /// set; drop the map entry once its last connection is gone.
    /// Called by the read loop on exit. No-op if the
    /// connection was already removed (e.g. by an identity rotation).
    fn reap_incoming(&mut self, peer: &Pubkey, stable_id: usize) {
        if let Entry::Occupied(mut slot) = self.incoming.entry(*peer) {
            let conns = slot.get_mut();
            conns.retain(|c| c.stable_id() != stable_id);
            if conns.is_empty() {
                slot.remove();
            }
        }
    }

    pub(crate) async fn run(mut self) {
        let mut prune = interval(BANLIST_PRUNE_INTERVAL);
        prune.set_missed_tick_behavior(MissedTickBehavior::Skip);

        let mut metrics = interval(METRICS_INTERVAL);
        metrics.set_missed_tick_behavior(MissedTickBehavior::Skip);

        // Gate for the accept branch: when armed, accept is disabled until the
        // sleep fires (at which point a fresh token should be available).
        let mut accept_gate = Box::pin(sleep(Duration::ZERO)); // starts open
        let mut gate_armed = false;

        // TODO: this flag is a workaround for some local-cluster tests that are a
        // nightmare to refactor. But they really should be.
        let mut id_closed = false;
        loop {
            tokio::select! {
                biased;
                changed = self.identity_rx.changed(), if !id_closed => {
                    if changed.is_err() {
                        warn!("identity rotation channel closed; inbound loop running without rotation support");
                        id_closed = true;
                        continue;
                    }
                    let snap = self.identity_rx.borrow_and_update().clone();
                    if let Some(snap) = snap {
                        self.apply_identity_change(snap);
                    }
                }
                // Lifecycle results keep the table coherent; drained above
                // accept so a flood of inbounds can't starve the reaping of
                // dead connections.
                Some(event) = self.events_rx.recv() => self.handle_event(event),
                // Metrics are quite handy to have even if we are flooded with incoming.
                _ = metrics.tick() => stats::report_server(&self.stats, self.incoming_len()),
                // Re-open the accept gate when the bucket has refilled.
                _ = &mut accept_gate, if gate_armed => {
                    gate_armed = false;
                }
                // We admit more load only when we're done with everything important,
                // and only as fast as the global handshake token bucket allows.
                maybe_incoming = self.endpoint.accept(), if !gate_armed => {
                    let Some(incoming) = maybe_incoming else { break };
                    let ip = incoming.remote_address().ip();
                    if !ip.is_loopback() && self.handshake_global_limiter.consume_tokens(1).is_err() {
                        add(&self.stats.handshake_rejected_global_limit);
                        let wait_us = self.handshake_global_limiter
                            .us_to_have_tokens(1)
                            .expect("requested amount is < bucket capacity");
                        let now = Instant::now();
                        accept_gate
                            .as_mut()
                            .reset(now.checked_add( Duration::from_micros(wait_us.max(1))).unwrap_or(now));
                        gate_armed = true;
                        incoming.ignore();
                    } else {
                        self.maybe_accept_connection(incoming);
                    }
                }
                // When idle we can take care of bookkeeping.
                _ = prune.tick() => self.banlist.prune(),
                // Shutdown is never done in a hurry
                _ = self.shutdown.cancelled() => break,
            }
        }
    }

    /// Rebuild the server TLS config against the new identity, swap it into the
    /// quinn endpoint, and evict the inbound table so peers re-handshake.
    fn apply_identity_change(&mut self, snap: Arc<IdentitySnapshot>) {
        let server_config =
            new_server_config(snap.cert.clone(), snap.key.clone_key(), ALPENGLOW_ALPN);
        self.endpoint.set_server_config(Some(server_config));
        // Bump first so any in-flight accept that completes after this point is
        // dropped at the event boundary (its event carries the old generation).
        self.generation = self.generation.wrapping_add(1);
        let evicted = self
            .incoming
            .drain()
            .flat_map(|(_, conns)| conns)
            .inspect(|conn| close_codes::IDENTITY_ROTATED.close(conn))
            .count() as u64;
        self.stats
            .connection_evicted_identity_rotated
            .fetch_add(evicted, Ordering::Relaxed);
        info!(
            "inbound identity rotated to {} ({} connection(s) evicted)",
            snap.pubkey, evicted
        );
    }

    /// Apply a connection-lifecycle result.
    fn handle_event(&mut self, event: InboundEvent) {
        let generation = match &event {
            InboundEvent::Accepted { generation, .. } | InboundEvent::Closed { generation, .. } => {
                *generation
            }
        };
        if generation != self.generation {
            // Close any live connection carried by a stale-generation event so a
            // connection authenticated under a now-rotated identity is not leaked.
            if let InboundEvent::Accepted { conn, .. } = event {
                close_codes::IDENTITY_ROTATED.close(&conn);
                self.stats
                    .connection_evicted_identity_rotated
                    .fetch_add(1, Ordering::Relaxed);
            }
            return;
        }
        match event {
            InboundEvent::Accepted { peer, conn, .. } => self.maybe_admit_inbound(peer, conn),
            InboundEvent::Closed {
                peer, stable_id, ..
            } => self.reap_incoming(&peer, stable_id),
        }
    }

    /// Performs the admission control checks of incoming connection, if
    /// they pass spawns the task to handle the handshake and serve connection.
    /// Caller is responsible for rate-limiting via the global handshake token bucket.
    fn maybe_accept_connection(&mut self, incoming: Incoming) {
        let remote_addr = incoming.remote_address();
        if remote_addr.is_ipv6() || remote_addr.ip().is_multicast() {
            incoming.ignore();
            return;
        }
        // TODO: add Retry challenge here.
        spawn(
            ServerConnection {
                incoming,
                generation: self.generation,
                events: self.events_tx.clone(),
                stats: self.stats.clone(),
            }
            .run(),
        );
    }

    /// Admission checks for a freshly handshaked inbound (we-accepted,
    /// receive-only) connection. The split-direction model has no lex-pubkey
    /// tiebreaker: we accept an inbound from any admitted peer regardless of
    /// pubkey ordering, and install it into the receive-only `incoming` map.
    fn maybe_admit_inbound(&mut self, peer: Pubkey, conn: Connection) {
        if self.banlist.is_banned(&peer) {
            close_codes::BANNED.close(&conn);
            record_error(&Error::Banned(peer), &self.stats);
            return;
        }
        if !self.allowlist.allow(&peer) {
            close_codes::NOT_ADMITTED.close(&conn);
            record_error(&Error::NotAdmitted(peer), &self.stats);
            return;
        }
        let remote_addr = conn.remote_address();
        if self.insert_inbound(peer, conn.clone()).is_err() {
            close_codes::TABLE_FULL.close(&conn);
            record_error(&Error::TableFull, &self.stats);
            return;
        }
        self.stats.record_connection_count(self.incoming_len());
        // The read loop reports [`InboundEvent::Closed`] when it exits so this
        // loop can reap the table slot.
        spawn(read_datagram_loop(
            conn,
            peer,
            remote_addr,
            self.generation,
            self.ingress.clone(),
            self.allowlist.clone(),
            self.banlist.clone(),
            self.events_tx.clone(),
            self.stats.clone(),
        ));
    }
}
