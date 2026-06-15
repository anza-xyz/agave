//! Inbound (server) direction: we-accept, receive-only.

use {
    crate::{
        ALLOWLIST_CHECK_INTERVAL, ALPENGLOW_ALPN, BANLIST_PRUNE_INTERVAL,
        MAX_DATAGRAMS_PER_SECOND_PER_PEER, MAX_INBOUND_CONNECTIONS_PER_PEER, METRICS_INTERVAL,
        PEER_RATE_LIMIT_BURST, PEER_RATE_LIMIT_BURST_DOS,
        allowlist::Allowlist,
        close_codes,
        endpoint::Datagram,
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

/// State for one peer
pub(crate) struct PeerEntry {
    conns: ArrayVec<Connection, MAX_INBOUND_CONNECTIONS_PER_PEER>,
    rate_limiter: Arc<TokenBucket>,
}

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
    /// The ingress traffic shaping bucket was drained by a sustained flood.
    FloodDetected { peer: Pubkey },
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
    rate_limiter: Arc<TokenBucket>,
    events: mpsc::Sender<InboundEvent>,
    stats: Arc<QuicDatagramStats>,
) {
    let stable_id = connection.stable_id();
    // Use the same bucket to be reused for both shaping and flood control
    const RATE_LIMIT_WATERMARK: u64 = PEER_RATE_LIMIT_BURST_DOS - PEER_RATE_LIMIT_BURST;
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
                                add(&stats.connection_lost);
                                let _ = events
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
                if banlist.is_banned(&peer) {
                    close_codes::BANNED.close(&connection);
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
    pub(crate) incoming: HashMap<Pubkey, PeerEntry, PubkeyHasherBuilder>,
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
        self.incoming.values().map(|e| e.conns.len()).sum::<usize>() as u64
    }

    /// Remove the connection with the given `stable_id` from `peer`'s inbound
    /// set; drop the map entry once its last connection is gone.
    /// Called by the read loop on exit. No-op if the
    /// connection was already removed (e.g. by an identity rotation).
    fn reap_incoming(&mut self, peer: &Pubkey, stable_id: usize) {
        if let Entry::Occupied(mut slot) = self.incoming.entry(*peer) {
            let conns = &mut slot.get_mut().conns;
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
        let mut accept_allowed = false;

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
                _ = &mut accept_gate, if accept_allowed => {
                    accept_allowed = false;
                }
                // We admit more load only when we're done with everything important,
                // and only as fast as the global handshake token bucket allows.
                maybe_incoming = self.endpoint.accept(), if !accept_allowed => {
                    let Some(incoming) = maybe_incoming else { break };
                    let ip = incoming.remote_address().ip();
                    if !ip.is_loopback() && self.handshake_global_limiter.consume_tokens(1).is_err() {
                        add(&self.stats.handshake_rejected_global_limit);
                        // us_to_have_tokens always succeeds, but we do not want
                        // panicking code here. Adding some min sleep to ensure
                        // we do not spin here.
                        let wait_us = self.handshake_global_limiter
                            .us_to_have_tokens(1).unwrap_or(0).clamp(100, 100_000);
                        accept_gate
                            .as_mut()
                            .reset(Instant::now().checked_add(Duration::from_micros(wait_us)).expect("Inputs should be bounded") );
                        accept_allowed = true;
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
            .flat_map(|(_, entry)| entry.conns)
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

    /// Apply a connection-lifecycle event.
    fn handle_event(&mut self, event: InboundEvent) {
        match event {
            // Stale Accepted: close the connection.
            InboundEvent::Accepted {
                generation, conn, ..
            } if generation != self.generation => {
                close_codes::IDENTITY_ROTATED.close(&conn);
                self.stats
                    .connection_evicted_identity_rotated
                    .fetch_add(1, Ordering::Relaxed);
            }
            // Relevant Accepted
            InboundEvent::Accepted { peer, conn, .. } => self.maybe_admit_inbound(peer, conn),
            // Stale Closed: no-op, table entry already gone.
            InboundEvent::Closed { generation, .. } if generation != self.generation => {}
            // Relevant Closed
            InboundEvent::Closed {
                peer, stable_id, ..
            } => self.reap_incoming(&peer, stable_id),
            // Flood detected - close connection
            InboundEvent::FloodDetected { peer } => {
                if let Some(entry) = self.incoming.remove(&peer) {
                    for conn in &entry.conns {
                        close_codes::BANNED.close(conn);
                    }
                    add(&self.stats.connection_lost);
                }
            }
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
        let rate_limiter = match self.incoming.entry(peer) {
            Entry::Vacant(slot) => {
                let rate_limiter = Arc::new(TokenBucket::new(
                    PEER_RATE_LIMIT_BURST_DOS,
                    PEER_RATE_LIMIT_BURST_DOS,
                    MAX_DATAGRAMS_PER_SECOND_PER_PEER,
                ));
                let mut conns = ArrayVec::new();
                conns.push(conn.clone());
                slot.insert(PeerEntry {
                    conns,
                    rate_limiter: rate_limiter.clone(),
                });
                rate_limiter
            }
            Entry::Occupied(mut slot) => {
                let entry = slot.get_mut();
                match entry.conns.try_push(conn.clone()) {
                    Ok(()) => Arc::clone(&entry.rate_limiter),
                    Err(_) => {
                        close_codes::TABLE_FULL.close(&conn);
                        record_error(&Error::TableFull, &self.stats);
                        return;
                    }
                }
            }
        };
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
            rate_limiter,
            self.events_tx.clone(),
            self.stats.clone(),
        ));
    }
}

#[cfg(test)]
mod tests {
    use {
        crate::{
            ALLOWLIST_CHECK_INTERVAL, MAX_DATAGRAMS_PER_SECOND_PER_PEER, PEER_RATE_LIMIT_BURST,
            allowlist::{AllowAll, StakedNodesAllowlist},
            endpoint::Datagram,
            testutils::{
                drain_matching, make_runtime, recv_until, send_until_received, spawn_node,
            },
        },
        bytes::Bytes,
        solana_keypair::{Keypair, Signer},
        std::{
            collections::HashMap,
            sync::{Arc, atomic::Ordering},
            time::Duration,
        },
    };

    #[test]
    fn test_staked_peer_is_admitted_unstaked_is_rejected() {
        let rt = make_runtime();

        let server_kp = Keypair::new();
        let a_kp = Keypair::new();
        let a_pk = a_kp.pubkey();
        let admit_map: HashMap<_, _> = std::iter::once((a_pk, 100)).collect();
        let server = spawn_node(
            &rt,
            Arc::new(StakedNodesAllowlist::new(admit_map)),
            server_kp,
        );

        // Client A - admitted by the allowlist.
        let client_a = spawn_node(&rt, Arc::new(AllowAll), a_kp);
        // Client B - not admitted by the allowlist.
        let client_b = spawn_node(&rt, Arc::new(AllowAll), Keypair::new());

        let payload_a = Bytes::from_static(b"hello-from-A");
        send_until_received(
            &rt,
            &client_a.endpoint,
            server.pubkey(),
            server.addr,
            payload_a.clone(),
            &server.ingress_rx,
            Duration::from_secs(5),
            |d| (d.peer_pubkey == a_pk && d.message == payload_a).then_some(()),
            "server never received payload from admitted peer A",
        );

        drain_matching(&server.ingress_rx, Duration::from_millis(200), |d| {
            d.message == payload_a
        });

        let payload_b = Bytes::from_static(b"hello-from-B");
        rt.block_on(async {
            client_b
                .endpoint
                .egress
                .send(Datagram {
                    peer_pubkey: server.pubkey(),
                    peer_address: server.addr,
                    message: payload_b.clone(),
                })
                .await
                .expect("egress send B");
        });

        // B's handshake completes, but the server rejects it at admission
        // (NOT_ADMITTED) before spawning a read loop, so B's trigger datagram is
        // never read into ingress.
        let bad = server.ingress_rx.recv_timeout(Duration::from_millis(800));
        assert!(
            bad.is_err(),
            "unstaked peer B's datagram should not reach server ingress, got {bad:?}"
        );
    }

    #[test]
    fn test_ban_evicts_existing_and_blocks_handshake() {
        let rt = make_runtime();
        let server = spawn_node(&rt, Arc::new(AllowAll), Keypair::new());
        let client = spawn_node(&rt, Arc::new(AllowAll), Keypair::new());

        // Establish a connection: the first send triggers a dial and the
        // trigger datagram is delivered once the handshake completes.
        let probe = Bytes::from_static(b"probe");
        send_until_received(
            &rt,
            &client.endpoint,
            server.pubkey(),
            server.addr,
            probe.clone(),
            &server.ingress_rx,
            Duration::from_secs(5),
            |d| (d.message == probe).then_some(()),
            "first datagram never arrived",
        );
        drain_matching(&server.ingress_rx, Duration::from_millis(200), |d| {
            d.message == probe
        });

        // Ban the client at the server side.
        server.banlist.ban(client.pubkey(), Duration::from_secs(60));

        // Repeatedly send: the first datagram rides the still-open connection and
        // is dropped post-read (BANNED close); subsequent ones trigger re-dials
        // that the server refuses at handshake. None may ever reach ingress.
        let again = Bytes::from_static(b"after-ban");
        for _ in 0..20 {
            let _ = client.endpoint.egress.try_send(Datagram {
                peer_pubkey: server.pubkey(),
                peer_address: server.addr,
                message: again.clone(),
            });
            if let Ok(d) = server.ingress_rx.recv_timeout(Duration::from_millis(100)) {
                assert_ne!(
                    d.message, again,
                    "banned client must not deliver datagrams to server"
                );
            }
        }
    }

    #[test]
    /// Two client instances sharing one identity (for hot-spare) each
    /// dial the server. Both inbound connections should *coexist* (capped at
    /// `MAX_INBOUND_CONNECTIONS_PER_PEER`). A third instance of the same
    /// identity is then refused with `TABLE_FULL`.
    fn test_two_inbound_same_identity_coexist_third_refused() {
        let rt = make_runtime();
        let server = spawn_node(&rt, Arc::new(AllowAll), Keypair::new());

        let shared = Keypair::new();
        let server_pk = server.pubkey();
        let c1 = spawn_node(&rt, Arc::new(AllowAll), shared.insecure_clone());
        let c2 = spawn_node(&rt, Arc::new(AllowAll), shared.insecure_clone());

        // C1 establishes its inbound to the server and delivers packet1.
        let packet1 = Bytes::from_static(b"from-c1");
        send_until_received(
            &rt,
            &c1.endpoint,
            server_pk,
            server.addr,
            packet1.clone(),
            &server.ingress_rx,
            Duration::from_secs(5),
            |d| (d.message == packet1).then_some(()),
            "server did not receive c1's probe",
        );
        drain_matching(&server.ingress_rx, Duration::from_millis(200), |d| {
            d.message == packet1
        });

        // C2 (same identity) brings up a *separate* inbound and delivers packet2.
        let packet2 = Bytes::from_static(b"from-c2");
        send_until_received(
            &rt,
            &c2.endpoint,
            server_pk,
            server.addr,
            packet2.clone(),
            &server.ingress_rx,
            Duration::from_secs(5),
            |d| (d.message == packet2).then_some(()),
            "server did not receive c2's probe",
        );
        drain_matching(&server.ingress_rx, Duration::from_millis(200), |d| {
            d.message == packet2
        });

        // No handover means no soft-ban on either client's outgoing side.
        std::thread::sleep(Duration::from_millis(300));

        // C1's connection survived C2 connecting (not displaced): it can still
        // deliver on the same outbound.
        let packet3 = Bytes::from_static(b"from-c1-again");
        send_until_received(
            &rt,
            &c1.endpoint,
            server_pk,
            server.addr,
            packet3.clone(),
            &server.ingress_rx,
            Duration::from_secs(5),
            |d| (d.message == packet3).then_some(()),
            "c1 must still deliver after c2's same-identity connection",
        );
        drain_matching(&server.ingress_rx, Duration::from_millis(200), |d| {
            d.message == packet3
        });

        // A THIRD instance of the same identity.
        let c3 = spawn_node(&rt, Arc::new(AllowAll), shared.insecure_clone());
        let blocked = Bytes::from_static(b"from-c3-table-full");
        for _ in 0..20 {
            // Each egress triggers a dial that handshakes, then is refused at
            // admission (TABLE_FULL) and closed.
            let _ = c3.endpoint.egress.try_send(Datagram {
                peer_pubkey: server_pk,
                peer_address: server.addr,
                message: blocked.clone(),
            });
            if let Ok(d) = server.ingress_rx.recv_timeout(Duration::from_millis(100)) {
                assert_ne!(
                    d.message, blocked,
                    "third same-identity inbound must be refused"
                );
            }
        }
    }

    #[test]
    /// A live inbound connection should be torn down when the peer drops out of the
    /// allowlist (e.g. at an epoch boundary).
    fn test_allowlist_eviction_closes_live_connection() {
        let rt = make_runtime();
        let client_kp = Keypair::new();
        let client_pk = client_kp.pubkey();
        let stakes: HashMap<_, _> = std::iter::once((client_pk, 100)).collect();
        let allowlist = Arc::new(StakedNodesAllowlist::new(stakes));
        let server = spawn_node(&rt, allowlist.clone(), Keypair::new());
        let client = spawn_node(&rt, Arc::new(AllowAll), client_kp);

        // Establish and deliver while the peer is admitted.
        let packet1 = Bytes::from_static(b"admitted");
        send_until_received(
            &rt,
            &client.endpoint,
            server.pubkey(),
            server.addr,
            packet1.clone(),
            &server.ingress_rx,
            Duration::from_secs(5),
            |d| (d.peer_pubkey == client_pk && d.message == packet1).then_some(()),
            "server never received datagram while peer admitted",
        );
        drain_matching(&server.ingress_rx, Duration::from_secs(1), |d| {
            d.message == packet1
        });

        // Remove the peer from the allowlist.
        allowlist.swap(Arc::default());

        // The periodic allowlist re-check is on a timer.
        std::thread::sleep(ALLOWLIST_CHECK_INTERVAL + Duration::from_secs(3));

        // Post-eviction the peer can no longer deliver: the old connection is
        // closed and any re-dial is refused at handshake.
        let packet2 = Bytes::from_static(b"after-eviction");
        for _ in 0..20 {
            let _ = client.endpoint.egress.try_send(Datagram {
                peer_pubkey: server.pubkey(),
                peer_address: server.addr,
                message: packet2.clone(),
            });
            if let Ok(d) = server.ingress_rx.recv_timeout(Duration::from_secs(1)) {
                assert_ne!(
                    d.message, packet2,
                    "evicted peer must not deliver after dropping out of the allowlist"
                );
            }
        }
    }

    #[test]
    /// Exercises the per-connection rate-limit corridor: a moderate burst
    /// above the refill rate is dropped without banning, the connection survives
    /// and resumes delivery after the bucket refills. A sustained flood that
    /// drains the whole bucket closes the connection but does NOT ban the peer.
    fn test_rate_limit_drops_then_closes_on_sustained_flood() {
        let rt = make_runtime();
        let server = spawn_node(&rt, Arc::new(AllowAll), Keypair::new());
        let client = spawn_node(&rt, Arc::new(AllowAll), Keypair::new());

        // Establish the connection: retry the probe until one lands.
        let probe = Bytes::from_static(b"probe");
        send_until_received(
            &rt,
            &client.endpoint,
            server.pubkey(),
            server.addr,
            probe.clone(),
            &server.ingress_rx,
            Duration::from_secs(5),
            |d| (d.message == probe).then_some(()),
            "first datagram never arrived",
        );
        // Drain probe retry duplicates so they don't inflate the post-burst
        // delivery count below.
        drain_matching(&server.ingress_rx, Duration::from_millis(200), |d| {
            d.message == probe
        });

        // Blast a burst well above the allowance
        // but far below the DoS threshold.
        let burst = (PEER_RATE_LIMIT_BURST as usize) * 4;
        rt.block_on(async {
            for i in 0..burst {
                let payload = Bytes::from(format!("burst-{i:04}").into_bytes());
                client
                    .endpoint
                    .egress
                    .send(Datagram {
                        peer_pubkey: server.pubkey(),
                        peer_address: server.addr,
                        message: payload,
                    })
                    .await
                    .unwrap();
            }
        });

        // Let the receiver chew through the backlog.
        std::thread::sleep(Duration::from_millis(500));

        assert!(
            !server.banlist.is_banned(&client.pubkey()),
            "client pubkey should NOT be banned by RX rate limit (drop-only semantics)"
        );

        // Drain whatever made it through ingress.
        let mut delivered = 0usize;
        while server
            .ingress_rx
            .recv_timeout(Duration::from_millis(50))
            .is_ok()
        {
            delivered = delivered.saturating_add(1);
        }
        let cap = PEER_RATE_LIMIT_BURST as usize;
        assert!(
            delivered <= cap + 10, // allow slack for possible refill while we send stuff
            "delivered {delivered} datagrams post-probe, exceeds green-corridor allowance {cap}"
        );

        // After waiting long enough for the bucket to refill, the sender should
        // be able to deliver fresh datagrams on the SAME connection
        let recover_secs = ((burst as u64).saturating_sub(PEER_RATE_LIMIT_BURST) as f64
            / MAX_DATAGRAMS_PER_SECOND_PER_PEER)
            .ceil() as u64;
        std::thread::sleep(Duration::from_secs(recover_secs.saturating_add(2)));
        let resume = Bytes::from_static(b"after-refill");
        rt.block_on(async {
            client
                .endpoint
                .egress
                .send(Datagram {
                    peer_pubkey: server.pubkey(),
                    peer_address: server.addr,
                    message: resume.clone(),
                })
                .await
                .unwrap();
        });
        recv_until(
            &server.ingress_rx,
            Duration::from_secs(5),
            |d| (d.message == resume).then_some(()),
            "post-refill datagram never arrived - connection may have been torn down",
        );
        let connections_lost_before = server
            .endpoint
            .server_stats
            .connection_lost
            .load(Ordering::Relaxed);

        // A SUSTAINED flood that drains the entire bucket must close the
        // connection but must NOT ban the peer.
        let flood = Bytes::from_static(b"flood");
        rt.block_on(async {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            loop {
                for _ in 0..2000 {
                    let _ = client
                        .endpoint
                        .egress
                        .send(Datagram {
                            peer_pubkey: server.pubkey(),
                            peer_address: server.addr,
                            message: flood.clone(),
                        })
                        .await;
                }
                let closed = server
                    .endpoint
                    .server_stats
                    .connection_lost
                    .load(Ordering::Relaxed);
                if closed > connections_lost_before {
                    break;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "timed out waiting for rate-limit connection close"
                );
            }
        });
        assert!(
            !server.banlist.is_banned(&client.pubkey()),
            "draining the per-connection DoS bucket must close the connection, not ban the peer"
        );
    }
}
