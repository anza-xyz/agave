//! QUIC datagram endpoint: public handle ([`QuicDatagramEndpoint`]) plus
//! its unified control loop ([`EndpointLoop`]).
use {
    crate::{
        Banlist, EGRESS_CHANNEL_CAP, MAX_PEERS,
        allowlist::Allowlist,
        close_codes,
        connection::{ClientConnection, ConnEvent, DialOutcome, ServerConnection},
        error::Error,
        handshake_throttle::HandshakeThrottle,
        peer_states::{
            EgressDispatch, IdGeneration, InsertOutcome, PeerConnectionState, PeerStates,
        },
        read_loop::read_datagram_loop,
        stats::{self, QuicDatagramStats, add, record_error},
        transport::{new_client_config, new_server_config},
    },
    bytes::Bytes,
    crossbeam_channel::Sender,
    log::{debug, info, warn},
    quinn::{Connection, Endpoint, EndpointConfig, Incoming, TokioRuntime},
    rustls::pki_types::{CertificateDer, PrivateKeyDer},
    solana_keypair::{Keypair, Signer},
    solana_pubkey::Pubkey,
    solana_tls_utils::{NotifyKeyUpdate, new_dummy_x509_certificate},
    std::{
        net::{SocketAddr, UdpSocket},
        sync::{Arc, atomic::Ordering},
        time::{Duration, Instant},
    },
    tokio::{
        sync::{mpsc, watch},
        task::JoinHandle,
        time::MissedTickBehavior,
    },
};

/// Identity material derived from a keypair: the ed25519 pubkey plus the
/// self-signed TLS cert/key that the endpoint presents to peers.
pub(crate) struct IdentitySnapshot {
    pub pubkey: Pubkey,
    pub cert: CertificateDer<'static>,
    pub key: PrivateKeyDer<'static>,
}

impl IdentitySnapshot {
    pub fn from_keypair(keypair: &Keypair) -> Self {
        let (cert, key) = new_dummy_x509_certificate(keypair);
        Self {
            pubkey: keypair.pubkey(),
            cert,
            key,
        }
    }
}

/// Handle for caller-driven identity rotation. Cloneable and thread-safe.
/// Implements [`NotifyKeyUpdate`] so it slots into the validator's existing
/// key-rotation plumbing.
pub struct KeyUpdater {
    tx: watch::Sender<Option<Arc<IdentitySnapshot>>>,
}

impl KeyUpdater {
    pub(crate) fn new() -> (Self, watch::Receiver<Option<Arc<IdentitySnapshot>>>) {
        let (tx, rx) = watch::channel(None);
        (Self { tx }, rx)
    }
}

impl NotifyKeyUpdate for KeyUpdater {
    fn update_key(&self, keypair: &Keypair) -> Result<(), Box<dyn std::error::Error>> {
        let snap = Arc::new(IdentitySnapshot::from_keypair(keypair));
        self.tx
            .send(Some(snap))
            .map_err(|_| -> Box<dyn std::error::Error> {
                "quic-datagram endpoint has shut down; identity update rejected".into()
            })?;
        Ok(())
    }
}

const BANLIST_PRUNE_INTERVAL: Duration = Duration::from_hours(1);
const METRICS_INTERVAL: Duration = Duration::from_secs(2);

/// Capacity of the task -> control-loop connection-event channel. Sized
/// generously so that an identity rotation - which closes up to
/// [`crate::MAX_PEERS`] connections, each of whose read loop then reliably reports
/// [`ConnEvent::Closed`] - drains without wedging.
const CONN_EVENT_CHANNEL_CAP: usize = MAX_PEERS as usize;

/// Datagram envelope used on both directions of the endpoint.
#[derive(Debug)]
pub struct Datagram {
    pub peer_pubkey: Pubkey,
    pub peer_address: SocketAddr,
    pub message: Bytes,
}

/// Datagram-only QUIC endpoint bound to UDP socket. The single control
/// loop task is spawned by [`Self::new`]; await `task` after
/// [`Self::close`] to observe full drain.
pub struct QuicDatagramEndpoint {
    endpoint: Endpoint,
    pub egress: mpsc::Sender<Datagram>,
    /// Handle for rotating the local identity (TLS cert / pubkey).
    pub key_updater: Arc<KeyUpdater>,
    pub task: JoinHandle<()>,
}

impl QuicDatagramEndpoint {
    /// Construct a datagram-only QUIC endpoint bound to `socket`. Spawns the
    /// unified control loop on `runtime`. Received datagrams flow into
    /// `ingress` via `try_send`; full ingress channel results in a drop
    /// (counted in `datagram_ingress_dropped_channel_full`).
    ///
    /// `allowlist` is consulted once per new connection in either direction.
    /// `banlist` is consulted on every send and at handshake.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        runtime: &tokio::runtime::Handle,
        keypair: &Keypair,
        socket: UdpSocket,
        alpn_protocol_id: &'static [u8],
        ingress: Sender<Datagram>,
        allowlist: Arc<dyn Allowlist>,
        banlist: Arc<Banlist<Pubkey>>,
    ) -> Result<Self, Error> {
        let local_pubkey = keypair.pubkey();
        let (cert, key) = new_dummy_x509_certificate(keypair);
        let server_config = new_server_config(cert.clone(), key.clone_key(), alpn_protocol_id);
        let client_config = new_client_config(cert, key, alpn_protocol_id);

        let mut endpoint = {
            // Endpoint::new requires being inside the runtime context, else it
            // panics on its first internal `tokio::spawn`.
            let _guard = runtime.enter();
            Endpoint::new(
                EndpointConfig::default(),
                Some(server_config),
                socket,
                Arc::new(TokioRuntime),
            )
            .map_err(Error::Endpoint)?
        };
        endpoint.set_default_client_config(client_config);

        let stats = Arc::<QuicDatagramStats>::default();
        let (egress_tx, egress_rx) = mpsc::channel(EGRESS_CHANNEL_CAP);
        let (conn_events_tx, conn_events_rx) = mpsc::channel(CONN_EVENT_CHANNEL_CAP);
        let (key_updater, identity_rx) = KeyUpdater::new();
        let key_updater = Arc::new(key_updater);

        let control = EndpointLoop {
            endpoint: endpoint.clone(),
            local_pubkey,
            generation: 0,
            egress_rx,
            ingress,
            banlist,
            allowlist,
            identity_rx,
            peers: PeerStates::new(),
            conn_events_tx,
            conn_events_rx,
            handshake_throttle: HandshakeThrottle::new(),
            stats,
            alpn: alpn_protocol_id,
        };
        let task = runtime.spawn(control.run());

        Ok(Self {
            endpoint,
            egress: egress_tx,
            key_updater,
            task,
        })
    }

    /// Initiate endpoint shutdown. The control loop exits on its next
    /// `endpoint.accept()` resolution (returns `None` once closed); in-flight
    /// connections are closed with `SHUTDOWN`. Callers should `await`
    /// [`Self::task`] to ensure all connections are terminated gracefully.
    pub fn close(&self) {
        self.endpoint
            .close(close_codes::SHUTDOWN.code, close_codes::SHUTDOWN.reason);
    }
}

struct EndpointLoop {
    endpoint: Endpoint,
    local_pubkey: Pubkey,
    /// Identity-rotation counter. Stamped into every spawned task and echoed
    /// back in its [`ConnEvent`]; [`Self::handle_conn_event`] drops any event
    /// whose generation no longer matches, so the table never has to reason
    /// about rotation itself.
    generation: IdGeneration,
    egress_rx: mpsc::Receiver<Datagram>,
    ingress: Sender<Datagram>,
    /// Policy consulted by the inbound admission gate and handed to each read
    /// loop for its periodic re-check.
    allowlist: Arc<dyn Allowlist>,
    /// Instand ban list, checked per packet.
    banlist: Arc<Banlist<Pubkey>>,
    identity_rx: watch::Receiver<Option<Arc<IdentitySnapshot>>>,
    /// The per-peer connection state machines.
    peers: PeerStates,
    /// Cloned into every spawned task so it can report its lifecycle result.
    conn_events_tx: mpsc::Sender<ConnEvent>,
    conn_events_rx: mpsc::Receiver<ConnEvent>,
    /// Bounds inbound handshakes to one in flight per source IP, so an
    /// allowlisted-but-misbehaving peer cannot flood handshake-CPU.
    handshake_throttle: HandshakeThrottle,
    stats: Arc<QuicDatagramStats>,
    /// Held so that an identity rotation can rebuild server + client TLS
    /// configs without revisiting the caller.
    alpn: &'static [u8],
}

impl EndpointLoop {
    async fn run(mut self) {
        let mut prune = tokio::time::interval(BANLIST_PRUNE_INTERVAL);
        prune.set_missed_tick_behavior(MissedTickBehavior::Skip);

        let mut metrics = tokio::time::interval(METRICS_INTERVAL);
        metrics.set_missed_tick_behavior(MissedTickBehavior::Skip);

        // The loop exits when egress / accept channels close - running a
        // half-broken endpoint after one of those has gone away is
        // pointless. The `identity_rx` arm is the lone exception: when
        // the `KeyUpdater`-side `watch::Sender` is dropped we disable the
        // arm via `id_closed` and keep running without identity rotation.
        //
        // TODO: this tolerance exists only to paper over `local-cluster`
        // passing a throwaway `Arc::new(RwLock::new(None))` for the
        // `admin_rpc_service_post_init` parameter to `Validator::new`.
        // That Arc drops the moment `Validator::new` returns, taking the
        // whole `Arc<KeyUpdaters> → Arc<KeyUpdater> → watch::Sender`
        // chain with it. Fix on the caller side is very invasive, so
        // was not applied.
        let mut id_closed = false;
        loop {
            tokio::select! {
                biased;
                // If identity is changed we should reconnect immediately
                // to maintain coherent state. Any existing backlog of packets
                // is probably invalid/irrelevant.
                changed = self.identity_rx.changed(), if !id_closed => {
                    if changed.is_err() {
                        // See TODO at top of loop
                        warn!("identity rotation channel closed; endpoint will run without rotation support");
                        id_closed = true;
                        continue;
                    }
                    let snap = self.identity_rx.borrow_and_update().clone();
                    if let Some(snap) = snap {
                        self.apply_identity_change(snap);
                    }
                }
                // egress to existing peers more important than accepting new
                maybe_datagram = self.egress_rx.recv() => {
                    let Some(datagram) = maybe_datagram else { break };
                    self.handle_datagram(datagram);
                }
                // Lifecycle results from spawned tasks keep the table coherent;
                // drained above accept so a flood of inbounds can't starve the
                // reaping of dead connections. `recv()` never yields `None` -
                // the loop itself holds a `conn_events_tx`.
                Some(event) = self.conn_events_rx.recv() => {
                    self.handle_conn_event(event);
                }
                maybe_incoming = self.endpoint.accept() => {
                    let Some(incoming) = maybe_incoming else { break };
                    self.accept_connection(incoming);
                }
                // when idle we can take care of bookkeeping. If these are delayed
                // it is usually not a problem.
                _ = prune.tick() => self.banlist.prune(),
                _ = metrics.tick() => stats::report(&self.stats, self.peers.len()),
            }
        }
    }

    /// Rebuild TLS configs against the new identity, swap them into the
    /// quinn endpoint, evict every cached connection so peers re-handshake,
    /// and adopt the new pubkey for the lex-direction rule going forward.
    fn apply_identity_change(&mut self, snap: Arc<IdentitySnapshot>) {
        let server_config = new_server_config(snap.cert.clone(), snap.key.clone_key(), self.alpn);
        let client_config = new_client_config(snap.cert.clone(), snap.key.clone_key(), self.alpn);

        self.local_pubkey = snap.pubkey;
        self.endpoint.set_default_client_config(client_config);
        self.endpoint.set_server_config(Some(server_config));

        // Bump first so any in-flight dial / accept that completes after this
        // point is dropped at the event boundary (its event carries the old
        // generation). Then wipe the table.
        self.generation = self.generation.wrapping_add(1);
        let evicted = self.peers.clear_for_id_change();
        self.stats
            .connection_evicted_identity_rotated
            .fetch_add(evicted, Ordering::Relaxed);
        info!(
            "identity rotated to {} ({} connection(s) evicted)",
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

        match self.peers.try_send(peer, addr, &bytes, &self.stats) {
            EgressDispatch::Sent | EgressDispatch::Dialing => return,
            EgressDispatch::SpawnDialTask => {}
        }

        // No usable connection yet. Carry the trigger packet into the dial task;
        // it is sent the moment the connection lands (`install_dialed`). This is
        // what lets a standstill broadcast reach a peer whose connection had died.
        // Followers arriving while `Dialing` is in the slot are dropped.
        ClientConnection {
            endpoint: self.endpoint.clone(),
            peer,
            addr,
            generation: self.generation,
            trigger: bytes,
            events: self.conn_events_tx.clone(),
            stats: self.stats.clone(),
        }
        .spawn();
    }

    /// Performs the non-expensive checks to handle incoming connections.
    /// Then spawns the statemachine to handle the handshake and serve connection.
    fn accept_connection(&mut self, incoming: Incoming) {
        let remote_addr = incoming.remote_address();
        if remote_addr.is_ipv6() || remote_addr.ip().is_multicast() {
            incoming.ignore();
            return;
        }
        if !incoming.remote_address_validated() {
            match incoming.retry() {
                Ok(()) => add(&self.stats.handshake_retry_sent),
                Err(e) => {
                    debug!("retry() failed for {remote_addr}");
                    e.into_incoming().ignore();
                }
            }
            return;
        }
        // Post-RETRY (source address is now return-routability-validated): the
        // *only* sources we will spend TLS-handshake CPU on are those whose IP
        // is a gossip-advertised address of a staked peer. Everything else is
        // refused here, before the handshake.
        // Loopback is trusted implicitly (local-cluster / tests).
        let ip = remote_addr.ip();
        if !ip.is_loopback() && !self.allowlist.allow_ip(&ip) {
            add(&self.stats.handshake_rejected_unknown_ip);
            incoming.refuse();
            return;
        }
        // Even an allowlisted source gets at most one in-flight handshake:
        // refuse a fresh start while a prior one from this IP could still be
        // running (loopback exempt).
        if !self.handshake_throttle.admit(ip, Instant::now()) {
            add(&self.stats.handshake_rejected_inflight);
            incoming.refuse();
            return;
        }
        ServerConnection {
            incoming,
            generation: self.generation,
            events: self.conn_events_tx.clone(),
            stats: self.stats.clone(),
        }
        .spawn();
    }

    /// Apply a connection-lifecycle result reported by a spawned task.
    fn handle_conn_event(&mut self, event: ConnEvent) {
        if event.generation() != self.generation {
            event.close_stale(&self.stats);
            return;
        }
        match event {
            ConnEvent::Inbound { peer, conn, .. } => self.maybe_admit_inbound(peer, conn),
            ConnEvent::DialDone { peer, outcome, .. } => match outcome {
                DialOutcome::Established { conn, trigger } => {
                    self.maybe_register_dialed(peer, conn, trigger)
                }
                DialOutcome::Failed => self.peers.clear_dialing_placeholder(&peer),
            },
            ConnEvent::Closed {
                peer, stable_id, ..
            } => self.peers.maybe_reap_connection(&peer, stable_id),
        }
    }

    /// Admission checks for a freshly handshaked inbound connection.
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
        match self.peers.insert_connection(
            peer,
            PeerConnectionState::Inbound(conn.clone()),
            &self.local_pubkey,
            &self.stats,
        ) {
            InsertOutcome::Inserted | InsertOutcome::Replaced => {
                self.stats.record_connection_count(self.peers.len());
                self.spawn_read_loop(conn, peer, remote_addr);
            }
            InsertOutcome::LexLoser => {
                // The canonical connection for this peer is already installed.
                close_codes::WRONG_DIRECTION.close(&conn);
            }
        }
    }

    /// Attempt to register a successfully dialed outbound connection,
    /// on success transition the `Dialing` placeholder to `Established`,
    /// send the trigger datagram, and start the read loop.
    fn maybe_register_dialed(&mut self, peer: Pubkey, conn: Connection, trigger: Bytes) {
        let remote_addr = conn.remote_address();
        match self.peers.insert_connection(
            peer,
            PeerConnectionState::Outbound(conn.clone()),
            &self.local_pubkey,
            &self.stats,
        ) {
            InsertOutcome::Inserted | InsertOutcome::Replaced => {
                self.stats.record_connection_count(self.peers.len());
                match conn.send_datagram(trigger) {
                    Ok(()) => add(&self.stats.datagrams_sent),
                    Err(e) => record_error(&Error::from(e), &self.stats),
                }
                self.spawn_read_loop(conn, peer, remote_addr);
            }
            // The canonical inbound connection arrived first.
            InsertOutcome::LexLoser => {
                close_codes::WRONG_DIRECTION.close(&conn);
                // try_send on an existing Inbound entry always returns Sent
                let result = self
                    .peers
                    .try_send(peer, remote_addr, &trigger, &self.stats);
                debug_assert!(
                    matches!(result, EgressDispatch::Sent),
                    "expected Sent: canonical Inbound connection must be present"
                );
            }
        }
    }

    /// Spawn the per-connection read loop for an installed connection. It
    /// reports [`ConnEvent::Closed`] when it exits so the loop can reap.
    fn spawn_read_loop(&self, conn: Connection, peer: Pubkey, remote_addr: SocketAddr) {
        tokio::spawn(read_datagram_loop(
            conn,
            peer,
            remote_addr,
            self.generation,
            self.ingress.clone(),
            self.allowlist.clone(),
            self.banlist.clone(),
            self.conn_events_tx.clone(),
            self.stats.clone(),
        ));
    }
}

#[cfg(test)]
mod tests {
    use {
        crate::{
            allowlist::AllowAll,
            testutils::{
                clone_keypair, drain_matching, keypair_below, make_runtime, recv_until,
                send_until_received, spawn_node,
            },
        },
        bytes::Bytes,
        solana_keypair::{Keypair, Signer},
        solana_tls_utils::NotifyKeyUpdate,
        std::{sync::Arc, time::Duration},
    };

    #[test]
    /// Both sides can initiate connections. The higher-pubkey node dials
    /// directly instead of waiting for the lower-pubkey peer to call in first.
    fn both_sides_can_initiate_connection() {
        let rt = make_runtime();
        // B has the higher pubkey, A the lower.
        let b = spawn_node(&rt, Arc::new(AllowAll), Keypair::new());
        let a = spawn_node(&rt, Arc::new(AllowAll), keypair_below(&b.pubkey()));

        // B (higher pubkey) sends to A (lower pubkey). B dials A directly.
        let from_b = Bytes::from_static(b"from-B-to-A");
        send_until_received(
            &rt,
            &b.endpoint,
            a.pubkey(),
            a.addr,
            from_b.clone(),
            &a.ingress_rx,
            Duration::from_secs(5),
            |d| (d.message == from_b).then_some(()),
            "A did not receive B's datagram",
        );
        drain_matching(&a.ingress_rx, Duration::from_millis(200), |d| {
            d.message == from_b
        });

        // A (lower pubkey) can also send to B.
        let from_a = Bytes::from_static(b"from-A-to-B");
        send_until_received(
            &rt,
            &a.endpoint,
            b.pubkey(),
            b.addr,
            from_a.clone(),
            &b.ingress_rx,
            Duration::from_secs(5),
            |d| (d.message == from_a).then_some(()),
            "B did not receive A's datagram",
        );
    }

    #[test]
    fn rotation_evicts_connections_and_resends_under_new_identity() {
        let rt = make_runtime();
        let server = spawn_node(&rt, Arc::new(AllowAll), Keypair::new());

        let k1 = keypair_below(&server.pubkey());
        let k1_pk = k1.pubkey();
        let client = spawn_node(&rt, Arc::new(AllowAll), k1);

        // Send under K1. Server should observe message attributed to K1.
        let p1 = Bytes::from_static(b"under-K1");
        send_until_received(
            &rt,
            &client.endpoint,
            server.pubkey(),
            server.addr,
            p1.clone(),
            &server.ingress_rx,
            Duration::from_secs(5),
            |d| (d.peer_pubkey == k1_pk && d.message == p1).then_some(()),
            "server never received message attributed to K1",
        );
        drain_matching(&server.ingress_rx, Duration::from_millis(200), |d| {
            d.message == p1
        });

        // Rotate to K2. Pick K2 also below the server so dial direction stays.
        let k2 = keypair_below(&server.pubkey());
        let k2_pk = k2.pubkey();
        assert_ne!(k1_pk, k2_pk, "K1 and K2 must differ");
        client
            .endpoint
            .key_updater
            .update_key(&k2)
            .expect("identity rotation accepted");

        // The control loop applies the rotation asynchronously: rebuild TLS
        // configs, evict cached connections (server sees IDENTITY_ROTATED).
        // Give it a beat.
        std::thread::sleep(Duration::from_millis(500));

        // Send under K2. Server's table no longer holds the K1 entry; a
        // fresh handshake under K2 establishes a new connection, and the
        // server observes the message attributed to K2.
        let p2 = Bytes::from_static(b"under-K2");
        send_until_received(
            &rt,
            &client.endpoint,
            server.pubkey(),
            server.addr,
            p2.clone(),
            &server.ingress_rx,
            Duration::from_secs(5),
            |d| (d.peer_pubkey == k2_pk && d.message == p2).then_some(()),
            "server never received message attributed to K2 after rotation",
        );

        // Client side: the rotated client should NOT have soft-banned the
        // server (rotation is a local event, not a HANDOVER-style takeover).
        assert!(
            !client.banlist.is_banned(&server.pubkey()),
            "rotation must not soft-ban peers we close ourselves"
        );
    }

    #[test]
    fn outbound_addr_change_redials_new_addr() {
        let rt = make_runtime();

        // S1 establishes the lex order. Client will be lex-lower.
        let s1 = spawn_node(&rt, Arc::new(AllowAll), Keypair::new());
        let s_key = clone_keypair(&s1.keypair);
        let s_pubkey = s1.pubkey();

        let client_kp = keypair_below(&s_pubkey);
        let client = spawn_node(&rt, Arc::new(AllowAll), client_kp);

        // Initial send: client dials S1 at its addr A1.
        let p1 = Bytes::from_static(b"p1");
        send_until_received(
            &rt,
            &client.endpoint,
            s_pubkey,
            s1.addr,
            p1.clone(),
            &s1.ingress_rx,
            Duration::from_secs(5),
            |d| (d.message == p1).then_some(()),
            "S1 did not receive p1",
        );
        drain_matching(&s1.ingress_rx, Duration::from_millis(200), |d| {
            d.message == p1
        });

        // S2 takes the same identity at a new addr - simulates a peer host
        // move with gossip publishing a new SocketAddr for the same pubkey.
        let s2 = spawn_node(&rt, Arc::new(AllowAll), s_key);
        assert_ne!(s1.addr, s2.addr, "S1 and S2 must bind distinct addrs");

        // Send to the new addr. Client must observe the addr mismatch,
        // evict its cached conn to A1, and re-dial A2.
        let p2 = Bytes::from_static(b"p2");
        send_until_received(
            &rt,
            &client.endpoint,
            s_pubkey,
            s2.addr,
            p2.clone(),
            &s2.ingress_rx,
            Duration::from_secs(5),
            |d| (d.message == p2).then_some(()),
            "S2 (post-move) did not receive p2",
        );

        // Stale conn to S1 was evicted; S1 must not see post-move datagrams.
        let stray = s1.ingress_rx.recv_timeout(Duration::from_millis(800));
        assert!(
            stray.is_err(),
            "S1 must not see post-move datagrams; got {stray:?}"
        );
    }

    #[test]
    fn inbound_connection_ignores_caller_addr_on_send() {
        // Inbound connections carry the peer's NAT-mapped source addr as their
        // remote_address, which may differ from the gossip-published addr the
        // caller provides. send_over_connection must use the cached inbound conn
        // regardless of the addr argument.
        let rt = make_runtime();

        let server = spawn_node(&rt, Arc::new(AllowAll), Keypair::new()); // lex-higher
        let client_kp = keypair_below(&server.pubkey());
        let client = spawn_node(&rt, Arc::new(AllowAll), client_kp);
        let c_pubkey = client.pubkey();

        // Client dials in so the server caches an inbound conn for c_pubkey.
        let probe = Bytes::from_static(b"open");
        send_until_received(
            &rt,
            &client.endpoint,
            server.pubkey(),
            server.addr,
            probe.clone(),
            &server.ingress_rx,
            Duration::from_secs(5),
            |d| (d.message == probe).then_some(()),
            "server did not receive client's probe",
        );

        // Server now sends back to client. The server already holds an inbound
        // connection for c_pubkey (from the client's dial), so send_over_connection
        // uses it without a new dial — single send + recv works here.
        // We deliberately hand a bogus addr to verify that inbound connections
        // ignore it on cache hit.
        let bogus_addr: std::net::SocketAddr = "203.0.113.99:65000".parse().unwrap();
        let pay = Bytes::from_static(b"reply");
        rt.block_on(async {
            server
                .endpoint
                .egress
                .send(crate::endpoint::Datagram {
                    peer_pubkey: c_pubkey,
                    peer_address: bogus_addr,
                    message: pay.clone(),
                })
                .await
                .unwrap();
        });
        recv_until(
            &client.ingress_rx,
            Duration::from_secs(5),
            |d| (d.message == pay).then_some(()),
            "client did not receive server's reply on the cached inbound conn",
        );
    }
}
