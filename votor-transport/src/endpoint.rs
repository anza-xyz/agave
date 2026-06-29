//! QUIC datagram endpoint
use {
    crate::{
        ALPENGLOW_ALPN, CONN_EVENT_CHANNEL_CAP, HANDSHAKE_BURST, HANDSHAKE_GLOBAL_RATE,
        HANDSHAKE_WORKERS_PER_ENDPOINT, MAX_INFLIGHT_HANDSHAKES, PeerListReceiver,
        client::OutboundLoop,
        error::Error,
        server::{AcceptLoop, InboundLoop},
        stats::ServerStats,
        transport::{Identity, new_client_config, new_server_config},
    },
    bytes::Bytes,
    crossbeam_channel::Sender,
    quinn::{Endpoint, EndpointConfig, TokioRuntime},
    solana_keypair::{Keypair, Signer},
    solana_net_utils::token_bucket::TokenBucket,
    solana_pubkey::Pubkey,
    solana_tls_utils::{NotifyKeyUpdate, new_dummy_x509_certificate},
    std::{
        net::{SocketAddr, UdpSocket},
        sync::Arc,
        time::Duration,
    },
    tokio::{
        runtime::Handle,
        sync::{mpsc, watch},
    },
    tokio_util::sync::CancellationToken,
};

/// Command to temporarily ban a peer.
pub struct BanCommand {
    pub peer: Pubkey,
    pub duration: Duration,
}

/// Handle for caller-driven identity rotation.
pub struct KeyUpdater {
    sender: watch::Sender<Option<Arc<Identity>>>,
}

impl NotifyKeyUpdate for KeyUpdater {
    fn update_key(&self, keypair: &Keypair) -> Result<(), Box<dyn std::error::Error>> {
        let new_identity = Arc::new(Identity::from_keypair(keypair));
        self.sender
            .send(Some(new_identity))
            .map_err(|_| -> Box<dyn std::error::Error> {
                "quic-datagram endpoint has shut down; identity update rejected".into()
            })?;
        Ok(())
    }
}

/// Datagram envelope used on both directions of the endpoint.
#[derive(Debug)]
pub struct Datagram {
    pub peer_pubkey: Pubkey,
    pub peer_address: SocketAddr,
    pub message: Bytes,
}

/// Datagram-only QUIC endpoint.
pub struct QuicDatagramEndpoint {
    /// Egress is broadcast: one queued message is fanned out to every live
    /// outbound connection.
    pub egress: mpsc::Sender<Bytes>,
    /// Handle for rotating the local identity (TLS cert / pubkey).
    pub key_updater: Arc<KeyUpdater>,
    shutdown: CancellationToken,
    /// Inbound counters, exposed so in-crate tests can assert on admission,
    /// eviction, and rate-limit behavior.
    #[cfg(test)]
    pub(crate) server_stats: Arc<ServerStats>,
}

impl QuicDatagramEndpoint {
    /// Construct a datagram-only QUIC endpoint. `inbound_sockets` back the
    /// inbound (we-accept) direction and are expected to be SO_REUSEPORT
    /// bound to the same port to load-balance inbound datagrams. The outbound
    /// (send-only) direction runs on a dedicated `outbound_socket` bound to its
    /// own port.
    ///
    /// Spawns the inbound and outbound loops on `runtime`; dropping the handle
    /// cancels them.
    /// Received datagrams flow into `inbound_datagrams`, per-peer receive rate is
    /// capped by `max_datagrams_per_second_per_peer`.
    /// `peer_list` carries desired peer set updates: inbound closes connections to
    /// peers no longer in the set, outbound connects to peers in it.
    /// `ban_commands` carries temporary per-peer ban commands (banning also closes
    /// the peer's connections).
    pub fn spawn(
        runtime: &Handle,
        keypair: &Keypair,
        inbound_sockets: Vec<UdpSocket>,
        outbound_socket: UdpSocket,
        inbound_datagrams: Sender<Datagram>,
        peer_list: PeerListReceiver,
        ban_commands: mpsc::Receiver<BanCommand>,
        max_datagrams_per_second_per_peer: usize,
    ) -> Result<Self, Error> {
        assert!(!inbound_sockets.is_empty(), "Must have sockets provided");

        let server_stats = Arc::new(ServerStats::default());
        // Egress channel carries *distinct* messages to be sent.
        // Size it to 5 seconds of the votor max send rate (these rates are quite low).
        let egress_channel_capacity = max_datagrams_per_second_per_peer.saturating_mul(5);
        let (egress_sender, egress_receiver) = mpsc::channel(egress_channel_capacity);
        let shutdown = CancellationToken::new();
        let (identity_sender, identity_receiver) = watch::channel(None);
        let key_updater = Arc::new(KeyUpdater {
            sender: identity_sender,
        });
        let local_pubkey = keypair.pubkey();

        let (cert, key) = new_dummy_x509_certificate(keypair);
        let server_config = new_server_config(cert.clone(), key.clone_key(), ALPENGLOW_ALPN);
        let client_config = new_client_config(cert, key, ALPENGLOW_ALPN);

        // Spawn a quinn endpoint for each socket.
        let (inbound_endpoints, mut outbound_endpoint) = {
            // Endpoint::new requires the runtime context.
            let _guard = runtime.enter();
            let inbound_endpoints = inbound_sockets
                .into_iter()
                .map(|socket| {
                    Endpoint::new(
                        EndpointConfig::default(),
                        Some(server_config.clone()),
                        socket,
                        Arc::new(TokioRuntime),
                    )
                    .map_err(Error::Endpoint)
                })
                .collect::<Result<Vec<_>, _>>()?;
            let outbound_endpoint = Endpoint::new(
                EndpointConfig::default(),
                None, // No server_config on this endpoint
                outbound_socket,
                Arc::new(TokioRuntime),
            )
            .map_err(Error::Endpoint)?;
            (inbound_endpoints, outbound_endpoint)
        };
        outbound_endpoint.set_default_client_config(client_config);

        let outbound = OutboundLoop::new(
            outbound_endpoint,
            local_pubkey,
            egress_receiver,
            identity_receiver.clone(),
            peer_list.clone(),
            shutdown.clone(),
        );
        runtime.spawn(outbound.run());

        // Inbound event channel allows the accept loops to forward authenticated
        // connections, and per-connection tasks report lifecycle events.
        let (inbound_events_sender, inbound_events_receiver) =
            mpsc::channel(CONN_EVENT_CHANNEL_CAP);
        // One accept loop per endpoint. Splits the global handshake budgets
        // evenly so the aggregate matches limits regardless of how many endpoints exist.
        let num_endpoints = inbound_endpoints.len();
        let handshake_burst = HANDSHAKE_BURST
            .checked_div(num_endpoints as u64)
            .expect("num_endpoints can not be zero");
        let max_inflight_handshakes = MAX_INFLIGHT_HANDSHAKES
            .checked_div(num_endpoints)
            .expect("num_endpoints can not be zero");
        let rate_limiter = TokenBucket::new(
            handshake_burst,
            handshake_burst,
            HANDSHAKE_GLOBAL_RATE / num_endpoints as f64,
        );
        for endpoint in &inbound_endpoints {
            for _ in 0..HANDSHAKE_WORKERS_PER_ENDPOINT {
                let accept = AcceptLoop::new(
                    endpoint.clone(),
                    inbound_events_sender.clone(),
                    server_stats.clone(),
                    shutdown.clone(),
                    rate_limiter.clone(),
                    max_inflight_handshakes,
                );
                runtime.spawn(accept.run());
            }
        }
        #[cfg(test)]
        let endpoint_server_stats = server_stats.clone();
        let inbound = InboundLoop::new(
            inbound_datagrams,
            ban_commands,
            peer_list,
            inbound_endpoints,
            inbound_events_sender,
            inbound_events_receiver,
            identity_receiver,
            server_stats,
            shutdown.clone(),
            max_datagrams_per_second_per_peer,
        );
        runtime.spawn(inbound.run());

        Ok(Self {
            egress: egress_sender,
            key_updater,
            shutdown,
            #[cfg(test)]
            server_stats: endpoint_server_stats,
        })
    }
}

impl Drop for QuicDatagramEndpoint {
    /// Cancel the spawned loops so a dropped handle can't leak them.
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use {
        super::{BanCommand, Datagram, QuicDatagramEndpoint},
        crate::{MAX_ALPENGLOW_VOTE_ACCOUNTS, PeerListSender},
        bytes::Bytes,
        crossbeam_channel::{Receiver, bounded},
        solana_keypair::{Keypair, Signer},
        solana_net_utils::sockets::bind_to_localhost_unique,
        solana_pubkey::Pubkey,
        solana_tls_utils::NotifyKeyUpdate,
        std::{
            collections::HashMap,
            net::{IpAddr, Ipv4Addr, SocketAddr},
            sync::{
                Arc,
                atomic::{AtomicU64, Ordering},
            },
            time::{Duration, Instant},
        },
        tokio::{
            runtime::{Builder, Runtime},
            sync::{mpsc, watch},
        },
    };

    /// Sentinel "no route" address: a peer_list entry with this address is
    /// admitted on the inbound side (membership only) but is never connected to
    /// Used for server nodes that should admit a peer without connecting back to it.
    const NO_ROUTE: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);

    const HIGH_PPS: usize = 1000;

    /// Ingress channel capacity. Mirrors `solana_core`'s `MAX_ALPENGLOW_PACKET_NUM`.
    const INGRESS_CAP: usize = 10_000;

    struct Node {
        endpoint: QuicDatagramEndpoint,
        ingress_receiver: Receiver<Datagram>,
        addr: SocketAddr,
        keypair: Keypair,
        peer_list_sender: PeerListSender,
        ban_sender: mpsc::Sender<BanCommand>,
    }

    impl Node {
        fn pubkey(&self) -> Pubkey {
            self.keypair.pubkey()
        }

        /// Broadcast `payload` to every peer in this node's peer_list.
        fn send(&self, payload: &Bytes) {
            self.endpoint
                .egress
                .try_send(payload.clone())
                .expect("This channel must never overflow");
        }

        fn set_peer_list(&self, map: HashMap<Pubkey, SocketAddr>) {
            self.peer_list_sender
                .send(Arc::new(map))
                .expect("peer_list receiver alive");
        }
    }

    fn make_runtime() -> Runtime {
        Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("tokio multi-thread runtime")
    }

    fn spawn_node(
        rt: &Runtime,
        keypair: Keypair,
        peer_list: HashMap<Pubkey, SocketAddr>,
        max_pps: usize,
    ) -> Node {
        let server_socket = bind_to_localhost_unique().expect("bind server UDP");
        let addr = server_socket.local_addr().expect("server local addr");
        let client_socket = bind_to_localhost_unique().expect("bind client UDP");
        // Channel sizes mirror prod (`solana_core::tvu`): ingress like
        // `MAX_ALPENGLOW_PACKET_NUM`, ban like `MAX_ALPENGLOW_VOTE_ACCOUNTS * 2`.
        let (ingress_sender, ingress_receiver) = bounded(INGRESS_CAP);
        let (ban_sender, ban_receiver) = mpsc::channel(MAX_ALPENGLOW_VOTE_ACCOUNTS * 2);
        let (peer_list_sender, peer_list_receiver) = watch::channel(Arc::new(peer_list));
        let endpoint = QuicDatagramEndpoint::spawn(
            rt.handle(),
            &keypair,
            vec![server_socket],
            client_socket,
            ingress_sender,
            peer_list_receiver,
            ban_receiver,
            max_pps,
        )
        .expect("QuicDatagramEndpoint::spawn");
        Node {
            endpoint,
            ingress_receiver,
            addr,
            keypair,
            peer_list_sender,
            ban_sender,
        }
    }

    /// A one-entry peer_list connecting to `peer` at `addr`.
    fn peer_list_of(peer: Pubkey, addr: SocketAddr) -> HashMap<Pubkey, SocketAddr> {
        std::iter::once((peer, addr)).collect()
    }

    /// Re-send `payload` until `cond` matches a received datagram or `timeout` elapses.
    /// Retrying covers the connect/handshake settling time.
    fn send_until_received<T>(
        sender: &Node,
        payload: &Bytes,
        receiver: &Receiver<Datagram>,
        timeout: Duration,
        mut cond: impl FnMut(&Datagram) -> Option<T>,
        msg: &str,
    ) -> T {
        let start = Instant::now();
        while start.elapsed() < timeout {
            sender.send(payload);
            if let Ok(item) = receiver.recv_timeout(Duration::from_millis(100))
                && let Some(t) = cond(&item)
            {
                return t;
            }
        }
        panic!("{msg}");
    }

    /// Drain datagrams matching `pred` (e.g. retry duplicates), stopping at the
    /// first non-matching datagram or once the channel is idle for `idle_gap`
    /// (i.e. the backlog has drained). Note: the first non-matching datagram is
    /// consumed (and discarded) before the loop breaks, so callers must not rely
    /// on it remaining in the channel.
    fn drain_matching(
        receiver: &Receiver<Datagram>,
        idle_gap: Duration,
        mut pred: impl FnMut(&Datagram) -> bool,
    ) {
        while let Ok(item) = receiver.recv_timeout(idle_gap) {
            if !pred(&item) {
                break;
            }
        }
    }

    /// Assert that, while repeatedly broadcasting `payload`, this *specific*
    /// payload never reaches `receiver` within the attempt budget. Datagrams
    /// with any other body are ignored.
    fn assert_not_delivered(
        sender: &Node,
        payload: &Bytes,
        receiver: &Receiver<Datagram>,
        attempts: u32,
    ) {
        for _ in 0..attempts {
            sender.send(payload);
            if let Ok(d) = receiver.recv_timeout(Duration::from_millis(150)) {
                assert_ne!(&d.message, payload, "payload must not be delivered");
            }
        }
    }

    /// Poll `stat` until it reaches at least `target` or `timeout` elapses,
    /// returning the final observed value. Replaces fixed sleeps before a
    /// single-shot stat assertion so the check does not race a slow scheduler.
    fn wait_for_stat(stat: &AtomicU64, target: u64, timeout: Duration) -> u64 {
        let start = Instant::now();
        loop {
            let value = stat.load(Ordering::Relaxed);
            if value >= target || start.elapsed() >= timeout {
                return value;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Basic exchange: each node lists the other in its peer_list, datagrams flow
    /// in both directions over two independent send-only connections.
    #[test]
    fn delivery_flows_both_directions() {
        let rt = make_runtime();
        let a_kp = Keypair::new();
        let b_kp = Keypair::new();
        let a_pk = a_kp.pubkey();
        let b_pk = b_kp.pubkey();
        let a = spawn_node(&rt, a_kp, HashMap::new(), HIGH_PPS);
        let b = spawn_node(&rt, b_kp, HashMap::new(), HIGH_PPS);
        a.set_peer_list(peer_list_of(b_pk, b.addr));
        b.set_peer_list(peer_list_of(a_pk, a.addr));

        let from_a = Bytes::from_static(b"from-A");
        send_until_received(
            &a,
            &from_a,
            &b.ingress_receiver,
            Duration::from_secs(10),
            |d| (d.peer_pubkey == a_pk && d.message == from_a).then_some(()),
            "B never received A's datagram",
        );
        let from_b = Bytes::from_static(b"from-B");
        send_until_received(
            &b,
            &from_b,
            &a.ingress_receiver,
            Duration::from_secs(10),
            |d| (d.peer_pubkey == b_pk && d.message == from_b).then_some(()),
            "A never received B's datagram",
        );
    }

    /// A peer in the server's peer_list is admitted; one that is not is rejected
    /// at admission (NOT_ADMITTED) and never reaches ingress.
    #[test]
    fn admitted_peer_delivers_unadmitted_is_rejected() {
        let rt = make_runtime();
        let a_kp = Keypair::new();
        let a_pk = a_kp.pubkey();
        let server = spawn_node(&rt, Keypair::new(), peer_list_of(a_pk, NO_ROUTE), HIGH_PPS);
        let server_pk = server.pubkey();
        let client_a = spawn_node(&rt, a_kp, peer_list_of(server_pk, server.addr), HIGH_PPS);
        let client_b = spawn_node(
            &rt,
            Keypair::new(),
            peer_list_of(server_pk, server.addr),
            HIGH_PPS,
        );

        let payload_a = Bytes::from_static(b"hello-from-A");
        send_until_received(
            &client_a,
            &payload_a,
            &server.ingress_receiver,
            Duration::from_secs(10),
            |d| (d.peer_pubkey == a_pk && d.message == payload_a).then_some(()),
            "server never received payload from admitted peer A",
        );
        drain_matching(&server.ingress_receiver, Duration::from_millis(300), |d| {
            d.message == payload_a
        });

        let payload_b = Bytes::from_static(b"hello-from-B");
        assert_not_delivered(&client_b, &payload_b, &server.ingress_receiver, 20);
        assert!(
            server
                .endpoint
                .server_stats
                .handshake_rejected_unauthorized
                .load(Ordering::Relaxed)
                > 0,
            "unadmitted peer B's handshake should have been rejected (unauthorized)"
        );
    }

    /// Banning a peer closes its live connection and
    /// blocks subsequent connections.
    #[test]
    fn ban_evicts_existing_and_blocks_handshake() {
        let rt = make_runtime();
        let client_keypair = Keypair::new();
        let client_pubkey = client_keypair.pubkey();
        let server = spawn_node(
            &rt,
            Keypair::new(),
            peer_list_of(client_pubkey, NO_ROUTE),
            HIGH_PPS,
        );
        let client = spawn_node(
            &rt,
            client_keypair,
            peer_list_of(server.pubkey(), server.addr),
            HIGH_PPS,
        );

        let probe = Bytes::from_static(b"probe");
        send_until_received(
            &client,
            &probe,
            &server.ingress_receiver,
            Duration::from_secs(10),
            |d| (d.message == probe).then_some(()),
            "first datagram never arrived",
        );
        // drain all packets from ingress
        drain_matching(&server.ingress_receiver, Duration::from_millis(300), |d| {
            d.message == probe
        });

        server
            .ban_sender
            .blocking_send(BanCommand {
                peer: client.pubkey(),
                duration: Duration::from_secs(60),
            })
            .expect("ban command accepted");

        // The connection was live (the probe was delivered), so the ban must
        // close it. This is the deterministic half of the behavior; poll for it
        // rather than racing a fixed sleep.
        let closed = wait_for_stat(
            &server.endpoint.server_stats.connection_closed_banned,
            1,
            Duration::from_secs(5),
        );
        assert!(closed >= 1, "ban must close the live connection");

        // And subsequent re-handshakes from the banned peer get nothing through.
        let after_ban = Bytes::from_static(b"after-ban");
        assert_not_delivered(&client, &after_ban, &server.ingress_receiver, 20);
    }

    /// Two client instances sharing one identity each bring up a *separate*
    /// inbound connection, a third same-identity instance is refused.
    #[test]
    fn two_inbound_same_identity_coexist_third_refused() {
        let rt = make_runtime();
        let shared = Keypair::new();
        let shared_pk = shared.pubkey();
        let server = spawn_node(
            &rt,
            Keypair::new(),
            peer_list_of(shared_pk, NO_ROUTE),
            HIGH_PPS,
        );
        let server_pk = server.pubkey();
        let peers = peer_list_of(server_pk, server.addr);
        let c1 = spawn_node(&rt, shared.insecure_clone(), peers.clone(), HIGH_PPS);
        let c2 = spawn_node(&rt, shared.insecure_clone(), peers.clone(), HIGH_PPS);

        let p1 = Bytes::from_static(b"from-c1");
        send_until_received(
            &c1,
            &p1,
            &server.ingress_receiver,
            Duration::from_secs(10),
            |d| (d.message == p1).then_some(()),
            "server did not receive c1's probe",
        );
        drain_matching(&server.ingress_receiver, Duration::from_millis(300), |d| {
            d.message == p1
        });
        let p2 = Bytes::from_static(b"from-c2");
        send_until_received(
            &c2,
            &p2,
            &server.ingress_receiver,
            Duration::from_secs(10),
            |d| (d.message == p2).then_some(()),
            "server did not receive c2's probe (second same-identity inbound)",
        );
        drain_matching(&server.ingress_receiver, Duration::from_millis(300), |d| {
            d.message == p2
        });

        let c3 = spawn_node(&rt, shared.insecure_clone(), peers.clone(), HIGH_PPS);
        let blocked = Bytes::from_static(b"from-c3-table-full");
        assert_not_delivered(&c3, &blocked, &server.ingress_receiver, 30);
        assert!(
            server
                .endpoint
                .server_stats
                .handshake_rejected_overload
                .load(Ordering::Relaxed)
                > 0,
            "third same-identity inbound should be refused (TooManyConnections)"
        );
    }

    /// Dropping a peer from the peer_list tears down *all* of its live inbound
    /// connections (one identity may hold up to `MAX_INBOUND_CONNECTIONS_PER_PEER`,
    /// i.e. 2), not just one. (e.g. at an epoch boundary.)
    #[test]
    fn peer_list_eviction_closes_live_connections() {
        let rt = make_runtime();
        // Two instances of one identity give the server two inbound connections.
        let shared = Keypair::new();
        let shared_pk = shared.pubkey();
        let server = spawn_node(
            &rt,
            Keypair::new(),
            peer_list_of(shared_pk, NO_ROUTE),
            HIGH_PPS,
        );
        let peers = peer_list_of(server.pubkey(), server.addr);
        let c1 = spawn_node(&rt, shared.insecure_clone(), peers.clone(), HIGH_PPS);
        let c2 = spawn_node(&rt, shared.insecure_clone(), peers.clone(), HIGH_PPS);

        let p1 = Bytes::from_static(b"from-c1");
        send_until_received(
            &c1,
            &p1,
            &server.ingress_receiver,
            Duration::from_secs(10),
            |d| (d.message == p1).then_some(()),
            "server never received c1's probe",
        );
        drain_matching(&server.ingress_receiver, Duration::from_millis(300), |d| {
            d.message == p1
        });
        let p2 = Bytes::from_static(b"from-c2");
        send_until_received(
            &c2,
            &p2,
            &server.ingress_receiver,
            Duration::from_secs(10),
            |d| (d.message == p2).then_some(()),
            "server never received c2's probe (second same-identity inbound)",
        );
        drain_matching(&server.ingress_receiver, Duration::from_millis(300), |d| {
            d.message == p2
        });

        server.set_peer_list(HashMap::new());
        // Poll for both closes rather than racing a fixed sleep.
        let closed = wait_for_stat(
            &server
                .endpoint
                .server_stats
                .connection_closed_not_in_peer_list,
            2,
            Duration::from_secs(5),
        );
        assert!(
            closed >= 2,
            "dropping the peer from the peer_list must close all (2) of its connections"
        );

        // Neither instance can deliver after eviction.
        let after = Bytes::from_static(b"after-eviction");
        assert_not_delivered(&c1, &after, &server.ingress_receiver, 20);
        assert_not_delivered(&c2, &after, &server.ingress_receiver, 20);
    }

    /// Per-connection rate-limit check: a burst above the refill rate is
    /// dropped and the connection survives. a sustained flood that drains
    /// the bucket dry closes the connection.
    #[test]
    fn rate_limit_drops_then_closes_on_sustained_flood() {
        // pps=20 => burst bucket = ceil(20*BURST_WINDOW=1s)=20,
        // DoS bucket = ceil(20*DOS_WINDOW=10s)=200.
        const PPS: usize = 20;
        const BURST: usize = 20;
        let rt = make_runtime();
        let client_keypair = Keypair::new();
        let client_pubkey = client_keypair.pubkey();
        let server = spawn_node(
            &rt,
            Keypair::new(),
            peer_list_of(client_pubkey, NO_ROUTE),
            PPS,
        );
        let client = spawn_node(
            &rt,
            client_keypair,
            peer_list_of(server.pubkey(), server.addr),
            PPS,
        );

        let probe = Bytes::from_static(b"probe");
        send_until_received(
            &client,
            &probe,
            &server.ingress_receiver,
            Duration::from_secs(10),
            |d| (d.message == probe).then_some(()),
            "first datagram never arrived",
        );
        drain_matching(&server.ingress_receiver, Duration::from_millis(300), |d| {
            d.message == probe
        });

        rt.block_on(async {
            for i in 0..(BURST * 4) {
                let payload = Bytes::from(format!("burst-{i:04}").into_bytes());
                let _ = client.endpoint.egress.send(payload).await;
            }
        });
        std::thread::sleep(Duration::from_secs(1));

        assert!(
            server
                .endpoint
                .server_stats
                .datagram_rate_limited
                .load(Ordering::Relaxed)
                > 0,
            "burst above the allowance should have the packets dropped"
        );

        let mut delivered = 0usize;
        while server
            .ingress_receiver
            .recv_timeout(Duration::from_millis(50))
            .is_ok()
        {
            delivered = delivered.saturating_add(1);
        }
        // BURST passes through immediately; on top of that the bucket keeps
        // refilling at PPS/sec during the send loop and the 1s settle window,
        // so a slow scheduler can let ~PPS more through. +30 is headroom over
        // BURST(=PPS=20) for that refill plus scheduling jitter.
        assert!(
            delivered <= BURST + 30,
            "delivered {delivered} datagrams post-probe, exceeds the allowance {BURST}"
        );

        // After the bucket refills, the SAME connection resumes delivery.
        std::thread::sleep(Duration::from_secs(10));
        let resume = Bytes::from_static(b"after-refill");
        send_until_received(
            &client,
            &resume,
            &server.ingress_receiver,
            Duration::from_secs(5),
            |d| (d.message == resume).then_some(()),
            "post-refill datagram never arrived",
        );

        // A sustained flood that drains the bucket dry closes the connection
        // (connection_lost grows).
        let lost_before = server
            .endpoint
            .server_stats
            .connection_lost
            .load(Ordering::Relaxed);
        let flood = Bytes::from_static(b"flood");
        rt.block_on(async {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            loop {
                for _ in 0..2000 {
                    let _ = client.endpoint.egress.send(flood.clone()).await;
                }
                if server
                    .endpoint
                    .server_stats
                    .connection_lost
                    .load(Ordering::Relaxed)
                    > lost_before
                {
                    break;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "timed out waiting for rate-limit connection close"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });
    }

    /// Rotating the CLIENT's identity evicts its outbound connections and
    /// reconnects under the new key, the server observes the post-rotation
    /// messages attributed to the new pubkey.
    #[test]
    fn client_identity_rotation_resends_under_new_identity() {
        let rt = make_runtime();
        let k1 = Keypair::new();
        let k1_pk = k1.pubkey();
        let k2 = Keypair::new();
        let k2_pk = k2.pubkey();
        assert_ne!(k1_pk, k2_pk, "K1 and K2 must differ");
        // Server admits both the old and new client identities so the rotated
        // client is still accepted after re-handshaking under K2.
        let server = spawn_node(
            &rt,
            Keypair::new(),
            HashMap::from([(k1_pk, NO_ROUTE), (k2_pk, NO_ROUTE)]),
            HIGH_PPS,
        );
        let client = spawn_node(
            &rt,
            k1,
            peer_list_of(server.pubkey(), server.addr),
            HIGH_PPS,
        );

        let p1 = Bytes::from_static(b"under-K1");
        send_until_received(
            &client,
            &p1,
            &server.ingress_receiver,
            Duration::from_secs(10),
            |d| (d.peer_pubkey == k1_pk && d.message == p1).then_some(()),
            "server never received message attributed to K1",
        );
        drain_matching(&server.ingress_receiver, Duration::from_millis(300), |d| {
            d.message == p1
        });

        client
            .endpoint
            .key_updater
            .update_key(&k2)
            .expect("identity rotation accepted");
        std::thread::sleep(Duration::from_millis(500));

        let p2 = Bytes::from_static(b"under-K2");
        send_until_received(
            &client,
            &p2,
            &server.ingress_receiver,
            Duration::from_secs(10),
            |d| (d.peer_pubkey == k2_pk && d.message == p2).then_some(()),
            "server never received message attributed to K2 after rotation",
        );
    }

    /// Rotating the SERVER's identity closes every inbound connection that was
    /// accepted under the old identity (close code IDENTITY_CHANGED).
    #[test]
    fn server_identity_rotation_evicts_inbound() {
        let rt = make_runtime();
        let client_keypair = Keypair::new();
        let client_pubkey = client_keypair.pubkey();
        let server = spawn_node(
            &rt,
            Keypair::new(),
            peer_list_of(client_pubkey, NO_ROUTE),
            HIGH_PPS,
        );
        let server_pubkey1 = server.pubkey();
        let client = spawn_node(
            &rt,
            client_keypair,
            peer_list_of(server_pubkey1, server.addr),
            HIGH_PPS,
        );

        let p1 = Bytes::from_static(b"under-server-id-1");
        send_until_received(
            &client,
            &p1,
            &server.ingress_receiver,
            Duration::from_secs(10),
            |d| (d.message == p1).then_some(()),
            "server never received datagram under original identity",
        );
        drain_matching(&server.ingress_receiver, Duration::from_millis(300), |d| {
            d.message == p1
        });

        let server_keypair2 = Keypair::new();
        let server_pubkey2 = server_keypair2.pubkey();
        assert_ne!(
            server_pubkey1, server_pubkey2,
            "server identities must differ"
        );
        server
            .endpoint
            .key_updater
            .update_key(&server_keypair2)
            .expect("server identity rotation must be accepted");
        let evicted = wait_for_stat(
            &server
                .endpoint
                .server_stats
                .connection_closed_identity_changed,
            1,
            Duration::from_secs(5),
        );
        assert!(
            evicted > 0,
            "server identity rotation must evict the inbound table"
        );

        // Datagrams to the OLD pubkey no longer arrive.
        let stale = Bytes::from_static(b"to-old-server-id");
        assert_not_delivered(&client, &stale, &server.ingress_receiver, 20);

        // The new identity is reachable at the same address once the client
        // peer_list is updated to it.
        client.set_peer_list(peer_list_of(server_pubkey2, server.addr));
        let p2 = Bytes::from_static(b"under-server-id-2");
        send_until_received(
            &client,
            &p2,
            &server.ingress_receiver,
            Duration::from_secs(10),
            |d| (d.message == p2).then_some(()),
            "server never received datagram under new identity",
        );
    }

    /// When a peer's address changes in the peer_list, the outbound
    /// loop closes the stale connection and connects to the new address.
    #[test]
    fn outbound_addr_change_reconnects_to_new_addr() {
        let rt = make_runtime();
        let server_keypair = Keypair::new();
        let server_pubkey = server_keypair.pubkey();
        let client_keypair = Keypair::new();
        let client_pubkey = client_keypair.pubkey();
        let server1 = spawn_node(
            &rt,
            server_keypair.insecure_clone(),
            peer_list_of(client_pubkey, NO_ROUTE),
            HIGH_PPS,
        );
        let client = spawn_node(
            &rt,
            client_keypair,
            peer_list_of(server_pubkey, server1.addr),
            HIGH_PPS,
        );

        let probe = Bytes::from_static(b"p1");
        send_until_received(
            &client,
            &probe,
            &server1.ingress_receiver,
            Duration::from_secs(10),
            |d| (d.message == probe).then_some(()),
            "server1 did not receive probe",
        );
        drain_matching(&server1.ingress_receiver, Duration::from_millis(300), |d| {
            d.message == probe
        });

        // Server takes the same identity at a new address (gossip publishes a move).
        let server2 = spawn_node(
            &rt,
            server_keypair.insecure_clone(),
            peer_list_of(client_pubkey, NO_ROUTE),
            HIGH_PPS,
        );
        assert_ne!(
            server1.addr, server2.addr,
            "S1 and S2 must bind distinct addrs"
        );
        client.set_peer_list(peer_list_of(server_pubkey, server2.addr));

        let probe2 = Bytes::from_static(b"p2");
        send_until_received(
            &client,
            &probe2,
            &server2.ingress_receiver,
            Duration::from_secs(10),
            |d| (d.message == probe2).then_some(()),
            "server2 did not receive probe2",
        );

        // The stale connection to server1 must be closed,
        // server1 must not see post-move data.
        let stray = server1
            .ingress_receiver
            .recv_timeout(Duration::from_millis(800));
        assert!(stray.is_err(), "Server1 must not see datagrams anymore!");
    }

    /// A node should never connect to itself.
    #[test]
    fn outbound_excludes_self() {
        let rt = make_runtime();
        let keypair = Keypair::new();
        let self_pubkey = keypair.pubkey();
        let node = spawn_node(&rt, keypair, HashMap::new(), HIGH_PPS);
        // Our own identity, mapped to our own inbound address.
        node.set_peer_list(peer_list_of(self_pubkey, node.addr));

        // Nothing loops back...
        let payload = Bytes::from_static(b"to-self");
        assert_not_delivered(&node, &payload, &node.ingress_receiver, 20);
        // ...and the inbound side never even saw a handshake: a self-connection would
        // have reached our own accept loop. With the connection skipped, every server
        // counter stays at zero.
        let stats = &node.endpoint.server_stats;
        assert_eq!(
            stats.handshakes_started.load(Ordering::Relaxed),
            0,
            "self-exclusion must skip the connection, no inbound handshake should start"
        );
        assert_eq!(
            stats.datagrams_received.load(Ordering::Relaxed),
            0,
            "no datagram should loop back to self"
        );
    }
}
