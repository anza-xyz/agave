//! QUIC datagram endpoint: public handle ([`QuicDatagramEndpoint`]) plus
//! its unified control loop ([`EndpointLoop`]). One tokio task handles
//! server accept, client egress, banlist eviction, identity rotation,
//! and metrics. Each event source is an arm of a single `tokio::select!`;
//! heavy per-event work is spawned onto its own task so a slow handshake
//! or dial does not block dispatch of the next event.

use {
    crate::{
        Banlist, EGRESS_CHANNEL_CAP,
        allowlist::Allowlist,
        client::ClientConnection,
        close_codes,
        connection_table::{ConnectionTable, EgressDispatch},
        error::Error,
        key_updater::{IdentitySnapshot, KeyUpdater},
        server::ServerConnection,
        stats::{self, QuicDatagramStats, add},
        subnet_rate_limit::SubnetRateLimiter,
        transport::{new_client_config, new_server_config},
    },
    bytes::Bytes,
    crossbeam_channel::Sender,
    log::{debug, info, warn},
    quinn::{Endpoint, EndpointConfig, Incoming, TokioRuntime},
    solana_keypair::{Keypair, Signer},
    solana_pubkey::Pubkey,
    solana_tls_utils::new_dummy_x509_certificate,
    std::{
        net::{SocketAddr, UdpSocket},
        sync::{Arc, atomic::Ordering},
        time::Duration,
    },
    tokio::{
        sync::{mpsc, watch},
        task::JoinHandle,
        time::MissedTickBehavior,
    },
};

const BANLIST_PRUNE_INTERVAL: Duration = Duration::from_secs(60 * 60);
const METRICS_INTERVAL: Duration = Duration::from_secs(2);

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
    pub endpoint: Endpoint,
    pub egress: mpsc::Sender<Datagram>,
    /// Handle for rotating the local identity (TLS cert / pubkey). Wraps a
    /// `tokio::sync::watch` channel; the actual swap happens asynchronously
    /// in the control loop. Implements `solana_tls_utils::NotifyKeyUpdate`
    /// so it slots into the validator's `KeyUpdaters` registry.
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
    pub fn new<A: Allowlist>(
        runtime: &tokio::runtime::Handle,
        keypair: &Keypair,
        socket: UdpSocket,
        alpn_protocol_id: &'static [u8],
        ingress: Sender<Datagram>,
        allowlist: Arc<A>,
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

        let table = Arc::new(ConnectionTable::new());
        let stats = Arc::<QuicDatagramStats>::default();
        let (egress_tx, egress_rx) = mpsc::channel(EGRESS_CHANNEL_CAP);
        let (key_updater, identity_rx) = KeyUpdater::new();
        let key_updater = Arc::new(key_updater);

        let control = EndpointLoop {
            endpoint: endpoint.clone(),
            local_pubkey,
            egress_rx,
            ingress,
            allowlist,
            banlist,
            identity_rx,
            connections: table,
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

struct EndpointLoop<A: Allowlist> {
    endpoint: Endpoint,
    local_pubkey: Pubkey,
    egress_rx: mpsc::Receiver<Datagram>,
    ingress: Sender<Datagram>,
    allowlist: Arc<A>,
    banlist: Arc<Banlist<Pubkey>>,
    identity_rx: watch::Receiver<Option<Arc<IdentitySnapshot>>>,
    connections: Arc<ConnectionTable>,
    stats: Arc<QuicDatagramStats>,
    /// Held so that an identity rotation can rebuild server + client TLS
    /// configs without revisiting the caller.
    alpn: &'static [u8],
}

impl<A: Allowlist> EndpointLoop<A> {
    async fn run(mut self) {
        let subnet_limit = Arc::new(SubnetRateLimiter::new());

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
                maybe_incoming = self.endpoint.accept() => {
                    let Some(incoming) = maybe_incoming else { break };
                    self.accept_connection(incoming, &subnet_limit);
                }
                // when idle we can take care of bookkeeping. If these are delayed
                // it is usually not a problem.
                _ = prune.tick() => self.banlist.prune(),
                _ = metrics.tick() => stats::report(&self.stats),
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

        let evicted = self.connections.clear_for_id_change();
        self.stats
            .connection_evicted_identity_rotated
            .fetch_add(evicted, Ordering::Relaxed);
        info!(
            "identity rotated to {} ({} connection(s) evicted)",
            snap.pubkey, evicted
        );
    }

    fn handle_datagram(&self, datagram: Datagram) {
        let Datagram {
            peer_pubkey: peer,
            peer_address: addr,
            message: bytes,
        } = datagram;
        debug_assert_ne!(self.local_pubkey, peer, "egress to self is a caller bug");
        if self.banlist.is_banned(&peer) {
            return;
        }
        // Lex rule partition connection directions: lex-higher side
        // only ever holds inbound (server-accepted) entries; lex-lower side
        // only ever holds outbound (we-dialed) ones.
        //
        //  - Higher side: trust whatever inbound conn we have. The peer's
        //    source addr can legitimately differ from the
        //    gossip-published addr due to gossip lag, so we ignore the
        //    addr argument on hit.
        //
        //  - Lower side: cached `Established`'s remote addr is exactly the
        //    addr we dialed. If the caller now wants a different addr (e.g.
        //    gossip refreshed the peer's published addr), the peer has
        //    moved - evict and re-dial. `Dialing` placeholder means another
        //    egress already kicked off a dial for this peer; drop this
        //    datagram rather than queueing.
        if self.local_pubkey >= peer {
            if self
                .connections
                .send_over_inbound_connection(&peer, &bytes, &self.stats)
            {
                return;
            }
            add(&self.stats.egress_dropped_higher_pubkey);
            return;
        }

        // Ask connection table if we should spawn a dialing task
        // retain the generation ID from it.
        let generation =
            match self
                .connections
                .send_over_outbound_connection(peer, addr, &bytes, &self.stats)
            {
                EgressDispatch::Sent | EgressDispatch::Dialing => return,
                EgressDispatch::SpawnDialTask { generation } => generation,
            };

        // Carry the trigger packet into the dial task; it'll be sent on
        // the new connection the moment `insert_connection` succeeds.
        // This is what lets a standstill broadcast (one cert every
        // `DELTA_STANDSTILL`) actually reach a peer whose connection
        // had died. Followers arriving during `Dialing` still drop on
        // the floor (see `dispatch_outbound`'s `Dialing` arm).
        ClientConnection {
            endpoint: self.endpoint.clone(),
            peer,
            addr,
            id_generation: generation,
            trigger: bytes,
            ingress: self.ingress.clone(),
            allowlist: self.allowlist.clone(),
            banlist: self.banlist.clone(),
            table: self.connections.clone(),
            stats: self.stats.clone(),
        }
        .spawn();
    }

    /// Performs the non-expensive checks to handle incoming connections.
    /// Then spawns the statemachine to handle the handshake and serve connection.
    fn accept_connection(&self, incoming: Incoming, subnet_limit: &Arc<SubnetRateLimiter>) {
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
        // Post-RETRY: Gate per-subnet here,
        // *before* spending CPU on the TLS handshake. An attacker with a
        // large IP pool can complete RETRY on each address but is bounded
        // by the per-/24 burst budget (100 attempts, refilling 1/min).
        if !subnet_limit.admit(remote_addr.ip()) {
            add(&self.stats.handshake_rejected_subnet_flood);
            incoming.refuse();
            return;
        }
        ServerConnection {
            incoming,
            local_pubkey: self.local_pubkey,
            ingress: self.ingress.clone(),
            allowlist: self.allowlist.clone(),
            banlist: self.banlist.clone(),
            table: self.connections.clone(),
            stats: self.stats.clone(),
        }
        .spawn();
    }
}

#[cfg(test)]
mod tests {
    use {
        crate::{
            allowlist::AllowAll,
            testutils::{
                clone_keypair, drain_matching, keypair_below, make_runtime, recv_until,
                send_until_received, spawn_node, spawn_node_with,
            },
        },
        bytes::Bytes,
        solana_keypair::Signer,
        solana_tls_utils::NotifyKeyUpdate,
        std::{sync::Arc, time::Duration},
    };

    #[test]
    /// When `local_pubkey >= peer` and no cached
    /// connection exists, the egress datagram is dropped (counted as
    /// `egress_dropped_higher_pubkey`). Once the lower-pubkey peer dials in,
    /// the higher side's egress flows via send_over_inbound on the cached inbound.
    fn higher_pubkey_drops_egress_until_lower_dials_in() {
        let rt = make_runtime();
        // B has the higher pubkey, A the lower.
        let b = spawn_node(&rt, Arc::new(AllowAll));
        let a = spawn_node_with(&rt, Arc::new(AllowAll), keypair_below(&b.pubkey()));

        // B (higher) tries to send to A (lower) before A has dialed B. Per the
        // lex rule, B's client drops the datagram silently - A must not receive.
        let dropped = Bytes::from_static(b"should-be-dropped");
        rt.block_on(async {
            b.endpoint
                .egress
                .send(crate::endpoint::Datagram {
                    peer_pubkey: a.pubkey(),
                    peer_address: a.addr,
                    message: dropped.clone(),
                })
                .await
                .unwrap();
        });
        let stray = a.ingress_rx.recv_timeout(Duration::from_millis(800));
        assert!(
            stray.is_err(),
            "lower-pubkey peer must not receive a higher-pubkey peer's egress without first \
             dialing in; got {stray:?}"
        );

        // A dials B (correct direction per the lex rule). B's server accepts
        // and caches the inbound from A. Trigger-drop means we need to retry
        // until the dial completes and a follower lands.
        let from_a = Bytes::from_static(b"from-A");
        send_until_received(
            &rt,
            &a.endpoint,
            b.pubkey(),
            b.addr,
            from_a.clone(),
            &b.ingress_rx,
            Duration::from_secs(5),
            |d| (d.message == from_a).then_some(()),
            "B did not receive A's dial",
        );
        drain_matching(&b.ingress_rx, Duration::from_millis(200), |d| {
            d.message == from_a
        });

        // B can now reach A via send_over_inbound on the cached inbound. No
        // dial needed — single send is enough.
        let from_b = Bytes::from_static(b"from-B-after-A-dialed");
        rt.block_on(async {
            b.endpoint
                .egress
                .send(crate::endpoint::Datagram {
                    peer_pubkey: a.pubkey(),
                    peer_address: a.addr,
                    message: from_b.clone(),
                })
                .await
                .unwrap();
        });
        recv_until(
            &a.ingress_rx,
            Duration::from_secs(5),
            |d| (d.message == from_b).then_some(()),
            "A did not receive B's datagram after the inbound established a connection",
        );
    }

    #[test]
    fn rotation_evicts_connections_and_resends_under_new_identity() {
        let rt = make_runtime();
        let server = spawn_node(&rt, Arc::new(AllowAll));

        let k1 = keypair_below(&server.pubkey());
        let k1_pk = k1.pubkey();
        let client = spawn_node_with(&rt, Arc::new(AllowAll), k1);

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
        let s1 = spawn_node(&rt, Arc::new(AllowAll));
        let s_key = clone_keypair(&s1.keypair);
        let s_pubkey = s1.pubkey();

        let client_kp = keypair_below(&s_pubkey);
        let client = spawn_node_with(&rt, Arc::new(AllowAll), client_kp);

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
        let s2 = spawn_node_with(&rt, Arc::new(AllowAll), s_key);
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
    fn higher_side_ignores_caller_addr_on_hit() {
        // Inbound (server-accepted) connections live on the lex-higher side.
        // Their cached `remote_address` is the peer's NAT-mapped source addr,
        // which the caller (working from gossip) may not know. The higher
        // side must trust the cached conn regardless of caller's addr.
        let rt = make_runtime();

        let server = spawn_node(&rt, Arc::new(AllowAll)); // lex-higher
        let client_kp = keypair_below(&server.pubkey());
        let client = spawn_node_with(&rt, Arc::new(AllowAll), client_kp);
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

        // Server now sends back to client. The server is lex-higher and
        // already holds an `Established` for c_pubkey (the inbound from
        // the client's dial), so `send_over_inbound` goes through the cached
        // connection without a dial — single send + recv works here.
        // We deliberately hand a bogus addr to verify the higher side
        // ignores it on cache hit.
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

    #[test]
    fn chained_address_changes_each_redial() {
        // Three servers sharing one identity, distinct addrs. Each egress to
        // a new addr evicts the prior `Established` with PEER_MOVED and dials
        // the new addr. Verifies the state machine can roll through
        // Established(A1) → Dialing → Established(A2) → Dialing →
        // Established(A3) without corruption.
        let rt = make_runtime();
        let s1 = spawn_node(&rt, Arc::new(AllowAll));
        let s_key = clone_keypair(&s1.keypair);
        let s_pubkey = s1.pubkey();
        let client = spawn_node_with(&rt, Arc::new(AllowAll), keypair_below(&s_pubkey));

        let s2 = spawn_node_with(&rt, Arc::new(AllowAll), clone_keypair(&s_key));
        let s3 = spawn_node_with(&rt, Arc::new(AllowAll), clone_keypair(&s_key));
        assert!(
            s1.addr != s2.addr && s2.addr != s3.addr && s1.addr != s3.addr,
            "the three servers must bind distinct addrs",
        );

        let p1 = Bytes::from_static(b"to-s1");
        let p2 = Bytes::from_static(b"to-s2");
        let p3 = Bytes::from_static(b"to-s3");

        send_until_received(
            &rt,
            &client.endpoint,
            s_pubkey,
            s1.addr,
            p1.clone(),
            &s1.ingress_rx,
            Duration::from_secs(5),
            |d| (d.message == p1).then_some(()),
            "s1 did not receive p1",
        );
        drain_matching(&s1.ingress_rx, Duration::from_millis(200), |d| {
            d.message == p1
        });

        send_until_received(
            &rt,
            &client.endpoint,
            s_pubkey,
            s2.addr,
            p2.clone(),
            &s2.ingress_rx,
            Duration::from_secs(5),
            |d| (d.message == p2).then_some(()),
            "s2 did not receive p2 - first address change failed",
        );
        drain_matching(&s2.ingress_rx, Duration::from_millis(200), |d| {
            d.message == p2
        });

        send_until_received(
            &rt,
            &client.endpoint,
            s_pubkey,
            s3.addr,
            p3.clone(),
            &s3.ingress_rx,
            Duration::from_secs(5),
            |d| (d.message == p3).then_some(()),
            "s3 did not receive p3 - chained address change failed",
        );

        // Each server saw only its targeted datagram (plus retry duplicates
        // we already drained), nothing else.
        let stray1 = s1.ingress_rx.recv_timeout(Duration::from_millis(500));
        assert!(
            stray1.is_err(),
            "s1 unexpectedly received extra {stray1:?} after move",
        );
        let stray2 = s2.ingress_rx.recv_timeout(Duration::from_millis(500));
        assert!(
            stray2.is_err(),
            "s2 unexpectedly received extra {stray2:?} after move",
        );
    }
}
