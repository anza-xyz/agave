//! QUIC datagram endpoint
use {
    crate::{
        CONN_EVENT_CHANNEL_CAP, HANDSHAKE_BURST, HANDSHAKE_GLOBAL_RATE,
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
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    },
    tokio::{
        runtime::Handle,
        sync::{mpsc, watch},
        task::JoinSet,
    },
    tokio_util::sync::CancellationToken,
};

pub(crate) const ENDPOINT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// Command to temporarily ban a peer.
pub struct BanCommand {
    pub peer: Pubkey,
    pub duration: Duration,
}

/// Envelope used to report incoming packets.
#[derive(Debug)]
pub struct Datagram {
    pub peer_pubkey: Pubkey,
    pub peer_address: SocketAddr,
    /// When the datagram was read off the connection.
    pub received_at: std::time::Instant,
    /// The connection's smoothed path RTT sampled at receive time.
    pub path_rtt: Duration,
    pub message: Bytes,
}

/// Datagram-only QUIC endpoint.
pub struct QuicDatagramEndpoint {
    /// Egress is broadcast: one queued message is fanned out to every live
    /// outbound connection.
    pub egress: mpsc::Sender<Bytes>,
    /// Handle for rotating the local identity (TLS cert / pubkey).
    pub key_updater: Arc<KeyUpdater>,
    exit_signals: ExitSignals,
    /// Spawned event-loop tasks. We must join these so a panic
    /// that tokio would otherwise swallow is surfaced.
    task_handles: JoinSet<()>,
    /// Inbound stats, exposed so integration tests can assert on them.
    #[cfg(any(test, feature = "dev-context-only-utils"))]
    pub server_stats: Arc<ServerStats>,
    #[cfg(test)]
    runtime_handle: Handle,
}

impl QuicDatagramEndpoint {
    /// Spawns the inbound and outbound loops on `runtime`.
    ///
    /// `inbound_sockets` back the inbound (we-accept) direction and are
    /// expected to be SO_REUSEPORT bound to the same port to load-balance.
    /// `outbound_socket` backs the outbound (send-only) direction bound to its
    /// own port.
    /// Received datagrams flow into `inbound_datagrams`, per-peer receive rate is
    /// capped by `max_datagrams_per_second_per_peer`.
    /// `peer_list` carries desired peer set: inbound closes connections to
    /// peers no longer in the set, outbound connects to peers in it.
    /// `ban_commands` carries temporary ban commands (banning also closes
    /// the peer's connections).
    /// `exit_signals` controls when the endpoint should terminate.
    /// `alpn` is the ALPN protocol identifier this endpoint speaks (e.g.
    /// [`crate::ALPENGLOW_ALPN`]); connections with a different ALPN are
    /// rejected during the TLS handshake.
    pub fn spawn(
        runtime: &Handle,
        keypair: &Keypair,
        alpn: &'static [u8],
        inbound_sockets: Vec<UdpSocket>,
        outbound_socket: UdpSocket,
        inbound_datagrams: Sender<Datagram>,
        peer_list: PeerListReceiver,
        ban_commands: mpsc::Receiver<BanCommand>,
        max_datagrams_per_second_per_peer: usize,
        exit_signals: ExitSignals,
    ) -> Result<Self, Error> {
        assert!(!inbound_sockets.is_empty(), "Must have sockets provided");

        let server_stats = Arc::new(ServerStats::default());
        // Egress channel carries *distinct* messages to be sent.
        // Size it to 5 seconds of the votor max send rate (these rates are quite low).
        let egress_channel_capacity = max_datagrams_per_second_per_peer.saturating_mul(5);
        let (egress_sender, egress_receiver) = mpsc::channel(egress_channel_capacity);
        let (identity_sender, identity_receiver) = watch::channel(None);
        let key_updater = Arc::new(KeyUpdater {
            sender: identity_sender,
        });
        let local_pubkey = keypair.pubkey();

        let (cert, key) = new_dummy_x509_certificate(keypair);
        let server_config = new_server_config(cert.clone(), key.clone_key(), alpn);
        let client_config = new_client_config(cert, key, alpn);

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
                None,
                outbound_socket,
                Arc::new(TokioRuntime),
            )
            .map_err(Error::Endpoint)?;
            (inbound_endpoints, outbound_endpoint)
        };
        outbound_endpoint.set_default_client_config(client_config);

        let mut task_handles = JoinSet::new();
        let outbound = OutboundLoop::new(
            outbound_endpoint,
            local_pubkey,
            alpn,
            egress_receiver,
            identity_receiver.clone(),
            peer_list.clone(),
            exit_signals.clone(),
        );
        task_handles.spawn_on(outbound.run(), runtime);

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
                    exit_signals.clone(),
                    rate_limiter.clone(),
                    max_inflight_handshakes,
                );
                task_handles.spawn_on(accept.run(), runtime);
            }
        }
        #[cfg(any(test, feature = "dev-context-only-utils"))]
        let endpoint_server_stats = server_stats.clone();
        let inbound = InboundLoop::new(
            inbound_datagrams,
            ban_commands,
            peer_list.clone(),
            inbound_endpoints,
            inbound_events_sender,
            inbound_events_receiver,
            identity_receiver,
            alpn,
            server_stats,
            exit_signals.clone(),
            max_datagrams_per_second_per_peer,
        );
        task_handles.spawn_on(inbound.run(), runtime);

        Ok(Self {
            egress: egress_sender,
            key_updater,
            exit_signals,
            task_handles,
            #[cfg(test)]
            runtime_handle: runtime.clone(),
            #[cfg(any(test, feature = "dev-context-only-utils"))]
            server_stats: endpoint_server_stats,
        })
    }

    pub async fn shutdown(&mut self) -> Result<(), String> {
        self.exit_signals.cancel();
        let deadline = tokio::time::Instant::now()
            .checked_add(ENDPOINT_SHUTDOWN_TIMEOUT)
            .expect("We are adding a small const duration");
        loop {
            match tokio::time::timeout_at(deadline, self.task_handles.join_next()).await {
                Ok(Some(Ok(()))) => {}
                Ok(Some(Err(join_err))) => {
                    if join_err.is_panic() {
                        return Err(format!("QuicDatagramEndpoint teardown error {join_err}"));
                    }
                }
                Ok(None) => break Ok(()),
                Err(_elapsed) => {
                    return Err("QuicDatagramEndpoint teardown: a task is stuck".to_string());
                }
            }
        }
    }
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

/// Aggregates both the validator-global exit atomic bool and the
/// validator-global cancellation token.
///
/// Some channels are held by sync services, we consult these to
/// tell a shutdown-time channel close from a bug.
#[derive(Clone)]
pub struct ExitSignals {
    exit: Arc<AtomicBool>,
    cancel: CancellationToken,
}

impl ExitSignals {
    /// `exit` is the validator-global exit flag; `cancel` should be a child of
    /// the validator-global cancellation token.
    pub fn new(exit: Arc<AtomicBool>, cancel: CancellationToken) -> Self {
        Self { exit, cancel }
    }

    pub(crate) fn is_exiting(&self) -> bool {
        self.exit.load(Ordering::Relaxed)
    }

    pub(crate) async fn cancelled(&self) {
        self.cancel.cancelled().await
    }

    pub(crate) fn cancel(&self) {
        self.cancel.cancel();
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use {
        super::{BanCommand, Datagram, ExitSignals, QuicDatagramEndpoint},
        crate::{
            MAX_ALPENGLOW_VOTE_ACCOUNTS, METRICS_INTERVAL, PeerListSender,
            transport::MAX_IDLE_TIMEOUT,
        },
        bytes::Bytes,
        crossbeam_channel::{Receiver, bounded},
        solana_keypair::{Keypair, Signer},
        solana_net_utils::sockets::{
            SocketConfiguration, bind_more_with_config, bind_to, bind_to_localhost_unique,
            unique_port_range_for_tests,
        },
        solana_pubkey::Pubkey,
        solana_tls_utils::NotifyKeyUpdate,
        std::{
            collections::HashMap,
            net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
            sync::{
                Arc,
                atomic::{AtomicBool, AtomicU64, Ordering},
            },
            time::{Duration, Instant},
        },
        tokio::{
            runtime::{Builder, Runtime},
            sync::{mpsc, watch},
        },
        tokio_util::sync::CancellationToken,
    };

    const HIGH_PPS: usize = 1000;

    fn make_runtime_for_tests() -> Runtime {
        Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("tokio multi-thread runtime")
    }

    /// Ingress channel capacity. Mirrors `solana_core`'s `MAX_ALPENGLOW_PACKET_NUM`.
    const INGRESS_CAP: usize = 10_000;

    struct Node {
        endpoint: QuicDatagramEndpoint,
        ingress_receiver: Receiver<Datagram>,
        addr: SocketAddr,
        keypair: Keypair,
        peer_list_sender: PeerListSender,
        ban_sender: mpsc::Sender<BanCommand>,
        exit_signals: ExitSignals,
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

        fn set_peer_list(&self, map: HashMap<Pubkey, Option<SocketAddr>>) {
            self.peer_list_sender
                .send(Arc::new(map))
                .expect("peer_list receiver alive");
        }

        fn spawn_node(
            rt: &Runtime,
            keypair: Keypair,
            peer_list: HashMap<Pubkey, Option<SocketAddr>>,
            max_pps: usize,
        ) -> Node {
            let server_socket = bind_to_localhost_unique().expect("bind server UDP");
            Node::spawn_node_with_sockets(rt, keypair, peer_list, max_pps, vec![server_socket])
        }

        /// As [`spawn_node`], but the caller supplies the inbound socket(s). Used to
        /// drive the multi-endpoint (SO_REUSEPORT) path where `inbound_sockets.len() > 1`.
        fn spawn_node_with_sockets(
            rt: &Runtime,
            keypair: Keypair,
            peer_list: HashMap<Pubkey, Option<SocketAddr>>,
            max_pps: usize,
            inbound_sockets: Vec<UdpSocket>,
        ) -> Node {
            let exit_signals =
                ExitSignals::new(Arc::new(AtomicBool::new(false)), CancellationToken::new());
            let addr = inbound_sockets[0]
                .local_addr()
                .expect("server local addr from first inbound socket");
            let client_socket = bind_to_localhost_unique().expect("bind client UDP");
            // Channel sizes mirror prod (`solana_core::tvu`): ingress like
            // `MAX_ALPENGLOW_PACKET_NUM`, ban like `MAX_ALPENGLOW_VOTE_ACCOUNTS * 2`.
            let (ingress_sender, ingress_receiver) = bounded(INGRESS_CAP);
            let (ban_sender, ban_receiver) = mpsc::channel(MAX_ALPENGLOW_VOTE_ACCOUNTS * 2);
            let (peer_list_sender, peer_list_receiver) = watch::channel(Arc::new(peer_list));
            let endpoint = QuicDatagramEndpoint::spawn(
                rt.handle(),
                &keypair,
                crate::ALPENGLOW_ALPN,
                inbound_sockets,
                client_socket,
                ingress_sender,
                peer_list_receiver,
                ban_receiver,
                max_pps,
                exit_signals.clone(),
            )
            .expect("QuicDatagramEndpoint::spawn");
            Node {
                endpoint,
                ingress_receiver,
                addr,
                keypair,
                peer_list_sender,
                ban_sender,
                exit_signals,
            }
        }
    }

    impl Drop for Node {
        fn drop(&mut self) {
            self.exit_signals.cancel();
            let handle = self.endpoint.runtime_handle.clone();
            handle
                .block_on(self.endpoint.shutdown())
                .expect("Shutdown should be clean");
        }
    }
    /// A one-entry peer_list connecting to `peer` at `addr`.
    fn peer_list_of(peer: Pubkey, addr: SocketAddr) -> HashMap<Pubkey, Option<SocketAddr>> {
        std::iter::once((peer, Some(addr))).collect()
    }

    /// A one-entry peer_list for `peer` whose address is not yet known.
    fn peer_list_with_unknown_addr(peer: Pubkey) -> HashMap<Pubkey, Option<SocketAddr>> {
        std::iter::once((peer, None)).collect()
    }

    /// Re-send `payload` until `cond` matches a received datagram or the delivery
    /// window elapses.
    fn send_until_received<T>(
        sender: &Node,
        payload: &Bytes,
        receiver: &Receiver<Datagram>,
        mut cond: impl FnMut(&Datagram) -> Option<T>,
    ) -> Result<T, ()> {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            sender.send(payload);
            if let Ok(item) = receiver.recv_timeout(Duration::from_millis(100))
                && let Some(t) = cond(&item)
            {
                return Ok(t);
            }
        }
        Err(())
    }

    /// Drain whatever datagrams are currently buffered (including retransmit
    /// duplicates of a just-received probe), returning once the channel has been
    /// idle long enough to conclude the backlog has cleared.
    fn drain_backlog(receiver: &Receiver<Datagram>) {
        while receiver.recv_timeout(Duration::from_millis(300)).is_ok() {}
    }

    /// Assert that, while repeatedly broadcasting `payload`, this specific
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
    /// returning the final observed value.
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

    #[test]
    fn test_client_keeps_trying() {
        let rt = make_runtime_for_tests();
        let keypair_a = Keypair::new();
        let keypair_b = Keypair::new();
        let pubkey_a = keypair_a.pubkey();
        let pubkey_b = keypair_b.pubkey();
        let node_a = Node::spawn_node(&rt, keypair_a, HashMap::new(), HIGH_PPS);
        let node_b = Node::spawn_node(&rt, keypair_b, HashMap::new(), HIGH_PPS);
        node_a.set_peer_list(peer_list_of(pubkey_b, node_b.addr));

        let from_a = Bytes::from_static(b"from-A");
        send_until_received(&node_a, &from_a, &node_b.ingress_receiver, |d| {
            (d.peer_pubkey == pubkey_a && d.message == from_a).then_some(())
        })
        .expect_err("B should not have received anything");
        // now allow A to connect
        node_b.set_peer_list(peer_list_of(pubkey_a, node_a.addr));
        send_until_received(&node_a, &from_a, &node_b.ingress_receiver, |d| {
            (d.peer_pubkey == pubkey_a && d.message == from_a).then_some(())
        })
        .expect("B never received A's datagram");
    }
    /// Basic exchange: each node lists the other in its peer_list, datagrams flow
    /// in both directions over two independent send-only connections.
    #[test]
    fn test_delivery_flows_both_directions() {
        let rt = make_runtime_for_tests();
        let keypair_a = Keypair::new();
        let keypair_b = Keypair::new();
        let pubkey_a = keypair_a.pubkey();
        let pubkey_b = keypair_b.pubkey();
        let node_a = Node::spawn_node(&rt, keypair_a, HashMap::new(), HIGH_PPS);
        let node_b = Node::spawn_node(&rt, keypair_b, HashMap::new(), HIGH_PPS);
        node_a.set_peer_list(peer_list_of(pubkey_b, node_b.addr));
        node_b.set_peer_list(peer_list_of(pubkey_a, node_a.addr));

        let from_a = Bytes::from_static(b"from-A");
        send_until_received(&node_a, &from_a, &node_b.ingress_receiver, |d| {
            (d.peer_pubkey == pubkey_a && d.message == from_a).then_some(())
        })
        .expect("B never received A's datagram");
        let from_b = Bytes::from_static(b"from-B");
        send_until_received(&node_b, &from_b, &node_a.ingress_receiver, |d| {
            (d.peer_pubkey == pubkey_b && d.message == from_b).then_some(())
        })
        .expect("A never received B's datagram");
    }

    /// A peer in the server's peer_list is admitted; one that is not is rejected
    /// at admission (NOT_ADMITTED) and never reaches ingress.
    #[test]
    fn test_server_admitted_peer_delivers_unadmitted_is_rejected() {
        let rt = make_runtime_for_tests();
        let keypair_a = Keypair::new();
        let pubkey_a = keypair_a.pubkey();
        let server = Node::spawn_node(
            &rt,
            Keypair::new(),
            peer_list_with_unknown_addr(pubkey_a),
            HIGH_PPS,
        );
        let server_pk = server.pubkey();
        let client_a = Node::spawn_node(
            &rt,
            keypair_a,
            peer_list_of(server_pk, server.addr),
            HIGH_PPS,
        );
        let client_b = Node::spawn_node(
            &rt,
            Keypair::new(),
            peer_list_of(server_pk, server.addr),
            HIGH_PPS,
        );

        let payload_a = Bytes::from_static(b"hello-from-A");
        send_until_received(&client_a, &payload_a, &server.ingress_receiver, |d| {
            (d.peer_pubkey == pubkey_a && d.message == payload_a).then_some(())
        })
        .expect("server never received payload from admitted peer A");
        drain_backlog(&server.ingress_receiver);

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

    /// Banning a peer closes its connections and blocks subsequent ones.
    #[test]
    fn test_server_ban_evicts_existing_and_blocks_handshake() {
        let rt = make_runtime_for_tests();
        let client_keypair = Keypair::new();
        let client_pubkey = client_keypair.pubkey();
        let server = Node::spawn_node(
            &rt,
            Keypair::new(),
            peer_list_with_unknown_addr(client_pubkey),
            HIGH_PPS,
        );
        let client = Node::spawn_node(
            &rt,
            client_keypair,
            peer_list_of(server.pubkey(), server.addr),
            HIGH_PPS,
        );

        let probe = Bytes::from_static(b"probe");
        send_until_received(&client, &probe, &server.ingress_receiver, |d| {
            (d.message == probe).then_some(())
        })
        .expect("first datagram never arrived");
        // drain all packets from ingress
        drain_backlog(&server.ingress_receiver);

        server
            .ban_sender
            .blocking_send(BanCommand {
                peer: client.pubkey(),
                duration: Duration::from_hours(1),
            })
            .expect("ban command accepted");

        // The connection was live (the probe was delivered), so the ban must
        // close it. This is the deterministic half of the behavior; poll for it
        // rather than racing a fixed sleep.
        let closed = wait_for_stat(
            &server.endpoint.server_stats.connection_closed_banned,
            1,
            METRICS_INTERVAL * 2,
        );
        assert!(closed >= 1, "ban must close the live connection");

        // And subsequent re-handshakes from the banned peer get nothing through.
        let after_ban = Bytes::from_static(b"after-ban");
        assert_not_delivered(&client, &after_ban, &server.ingress_receiver, 20);
    }

    /// Two client instances sharing one identity each bring up a separate
    /// inbound connection, a third same-identity instance is refused.
    #[test]
    fn test_server_two_inbound_same_identity_coexist_third_refused() {
        let rt = make_runtime_for_tests();
        let shared = Keypair::new();
        let shared_pk = shared.pubkey();
        let server = Node::spawn_node(
            &rt,
            Keypair::new(),
            peer_list_with_unknown_addr(shared_pk),
            HIGH_PPS,
        );
        let server_pk = server.pubkey();
        // Keep `handshake_rejected_overload` cumulative across report ticks.
        server
            .endpoint
            .server_stats
            .report_frozen
            .store(true, Ordering::Relaxed);
        let peers = peer_list_of(server_pk, server.addr);
        let client1 = Node::spawn_node(&rt, shared.insecure_clone(), peers.clone(), HIGH_PPS);
        let client2 = Node::spawn_node(&rt, shared.insecure_clone(), peers.clone(), HIGH_PPS);

        let payload1 = Bytes::from_static(b"from-c1");
        send_until_received(&client1, &payload1, &server.ingress_receiver, |d| {
            (d.message == payload1).then_some(())
        })
        .expect("server did not receive c1's probe");
        drain_backlog(&server.ingress_receiver);
        let payload2 = Bytes::from_static(b"from-c2");
        send_until_received(&client2, &payload2, &server.ingress_receiver, |d| {
            (d.message == payload2).then_some(())
        })
        .expect("server did not receive c2's probe (second same-identity inbound)");
        drain_backlog(&server.ingress_receiver);

        let client3 = Node::spawn_node(&rt, shared.insecure_clone(), peers.clone(), HIGH_PPS);
        let blocked = Bytes::from_static(b"from-c3-table-full");
        assert_not_delivered(&client3, &blocked, &server.ingress_receiver, 30);
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

    /// Dropping a peer from peer_list tears down all of its inbound connections.
    #[test]
    fn test_server_peer_list_eviction_closes_live_connections() {
        let rt = make_runtime_for_tests();
        // Two instances of one identity give the server two inbound connections.
        let shared = Keypair::new();
        let shared_pk = shared.pubkey();
        let server = Node::spawn_node(
            &rt,
            Keypair::new(),
            peer_list_with_unknown_addr(shared_pk),
            HIGH_PPS,
        );
        let peers = peer_list_of(server.pubkey(), server.addr);
        let client1 = Node::spawn_node(&rt, shared.insecure_clone(), peers.clone(), HIGH_PPS);
        let client2 = Node::spawn_node(&rt, shared.insecure_clone(), peers.clone(), HIGH_PPS);

        let payload1 = Bytes::from_static(b"from-c1");
        send_until_received(&client1, &payload1, &server.ingress_receiver, |d| {
            (d.message == payload1).then_some(())
        })
        .expect("server never received c1's probe");
        drain_backlog(&server.ingress_receiver);
        let payload2 = Bytes::from_static(b"from-c2");
        send_until_received(&client2, &payload2, &server.ingress_receiver, |d| {
            (d.message == payload2).then_some(())
        })
        .expect("server never received c2's probe (second same-identity inbound)");
        drain_backlog(&server.ingress_receiver);

        server.set_peer_list(HashMap::new());
        // Poll for both closes rather than racing a fixed sleep.
        let closed = wait_for_stat(
            &server
                .endpoint
                .server_stats
                .connection_closed_not_in_peer_list,
            2,
            METRICS_INTERVAL * 2,
        );
        assert!(
            closed >= 2,
            "dropping the peer from the peer_list must close all (2) of its connections"
        );

        // Neither instance can deliver after eviction.
        let after = Bytes::from_static(b"after-eviction");
        assert_not_delivered(&client1, &after, &server.ingress_receiver, 20);
        assert_not_delivered(&client2, &after, &server.ingress_receiver, 20);
    }

    /// Peer rate-limit check: a burst above the refill rate is
    /// dropped and the connection survives. A sustained flood that drains
    /// the bucket dry closes the connection.
    #[test]
    fn test_server_rate_limit_drops_then_closes_on_sustained_flood() {
        // pps=20 => burst bucket = ceil(20*BURST_WINDOW=1s)=20,
        // DoS bucket = ceil(20*DOS_WINDOW=10s)=200.
        const PPS: usize = 20;
        const BURST: usize = 20;
        let rt = make_runtime_for_tests();
        let client_keypair = Keypair::new();
        let client_pubkey = client_keypair.pubkey();
        let server = Node::spawn_node(
            &rt,
            Keypair::new(),
            peer_list_with_unknown_addr(client_pubkey),
            PPS,
        );
        // Keep `datagram_rate_limited` / `connection_lost` cumulative across report ticks.
        server
            .endpoint
            .server_stats
            .report_frozen
            .store(true, Ordering::Relaxed);
        let client = Node::spawn_node(
            &rt,
            client_keypair,
            peer_list_of(server.pubkey(), server.addr),
            PPS,
        );

        let probe = Bytes::from_static(b"probe");
        send_until_received(&client, &probe, &server.ingress_receiver, |d| {
            (d.message == probe).then_some(())
        })
        .expect("first datagram never arrived");
        drain_backlog(&server.ingress_receiver);

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
        send_until_received(&client, &resume, &server.ingress_receiver, |d| {
            (d.message == resume).then_some(())
        })
        .expect("post-refill datagram never arrived");

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

    /// Changing the client identity closes its outbound connections and
    /// reconnects to all peers.
    #[test]
    fn test_client_identity_rotation_resends_under_new_identity() {
        let rt = make_runtime_for_tests();
        let keypair1 = Keypair::new();
        let pubkey1 = keypair1.pubkey();
        let keypair2 = Keypair::new();
        let pubkey2 = keypair2.pubkey();
        // Server admits both the old and new client identities so the rotated
        // client is still accepted after re-handshaking under K2.
        let server = Node::spawn_node(
            &rt,
            Keypair::new(),
            HashMap::from([(pubkey1, None), (pubkey2, None)]),
            HIGH_PPS,
        );
        let client = Node::spawn_node(
            &rt,
            keypair1,
            peer_list_of(server.pubkey(), server.addr),
            HIGH_PPS,
        );

        let payload1 = Bytes::from_static(b"under-K1");
        send_until_received(&client, &payload1, &server.ingress_receiver, |d| {
            (d.peer_pubkey == pubkey1 && d.message == payload1).then_some(())
        })
        .expect("server never received message attributed to K1");
        drain_backlog(&server.ingress_receiver);

        client
            .endpoint
            .key_updater
            .update_key(&keypair2)
            .expect("identity rotation accepted");
        std::thread::sleep(Duration::from_millis(500));

        let payload2 = Bytes::from_static(b"under-K2");
        send_until_received(&client, &payload2, &server.ingress_receiver, |d| {
            (d.peer_pubkey == pubkey2 && d.message == payload2).then_some(())
        })
        .expect("server never received message attributed to K2 after rotation");
    }

    /// Changing the server identity closes every inbound connection that was
    /// accepted under the old identity (close code IDENTITY_CHANGED).
    #[test]
    fn test_server_identity_rotation_evicts_inbound() {
        let rt = make_runtime_for_tests();
        let client_keypair = Keypair::new();
        let client_pubkey = client_keypair.pubkey();
        let server = Node::spawn_node(
            &rt,
            Keypair::new(),
            peer_list_with_unknown_addr(client_pubkey),
            HIGH_PPS,
        );
        let server_pubkey1 = server.pubkey();
        let client = Node::spawn_node(
            &rt,
            client_keypair,
            peer_list_of(server_pubkey1, server.addr),
            HIGH_PPS,
        );

        let payload1 = Bytes::from_static(b"under-server-id-1");
        send_until_received(&client, &payload1, &server.ingress_receiver, |d| {
            (d.message == payload1).then_some(())
        })
        .expect("server never received datagram under original identity");
        drain_backlog(&server.ingress_receiver);

        let server_keypair2 = Keypair::new();
        let server_pubkey2 = server_keypair2.pubkey();
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
            METRICS_INTERVAL * 2,
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
        let payload2 = Bytes::from_static(b"under-server-id-2");
        send_until_received(&client, &payload2, &server.ingress_receiver, |d| {
            (d.message == payload2).then_some(())
        })
        .expect("server never received datagram under new identity");
    }

    /// When a peer's address changes in the peer_list, the outbound
    /// loop closes the stale connection and connects to the new address.
    #[test]
    fn test_client_addr_change_reconnects_to_new_addr() {
        let rt = make_runtime_for_tests();
        let server_keypair = Keypair::new();
        let server_pubkey = server_keypair.pubkey();
        let client_keypair = Keypair::new();
        let client_pubkey = client_keypair.pubkey();
        let server1 = Node::spawn_node(
            &rt,
            server_keypair.insecure_clone(),
            peer_list_with_unknown_addr(client_pubkey),
            HIGH_PPS,
        );
        let client = Node::spawn_node(
            &rt,
            client_keypair,
            peer_list_of(server_pubkey, server1.addr),
            HIGH_PPS,
        );

        let probe = Bytes::from_static(b"p1");
        send_until_received(&client, &probe, &server1.ingress_receiver, |d| {
            (d.message == probe).then_some(())
        })
        .expect("server1 did not receive probe");
        drain_backlog(&server1.ingress_receiver);

        // Server takes the same identity at a new address (gossip publishes a move).
        let server2 = Node::spawn_node(
            &rt,
            server_keypair.insecure_clone(),
            peer_list_with_unknown_addr(client_pubkey),
            HIGH_PPS,
        );
        client.set_peer_list(peer_list_of(server_pubkey, server2.addr));

        let probe2 = Bytes::from_static(b"p2");
        send_until_received(&client, &probe2, &server2.ingress_receiver, |d| {
            (d.message == probe2).then_some(())
        })
        .expect("server2 did not receive probe2");

        // The stale connection to server1 must be closed,
        // server1 must not see post-move data.
        let stray = server1
            .ingress_receiver
            .recv_timeout(Duration::from_millis(800));
        assert!(stray.is_err(), "Server1 must not see datagrams anymore!");
    }

    #[test]
    fn test_client_connections_excludes_self() {
        let rt = make_runtime_for_tests();
        let keypair = Keypair::new();
        let self_pubkey = keypair.pubkey();
        let node = Node::spawn_node(&rt, keypair, HashMap::new(), HIGH_PPS);
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

    /// Background GC: after an inbound connection closes, the peer entry lingers as
    /// a tombstone (the per-peer rate limiter is retained in case the peer reconnects
    /// quickly). Once that limiter refills, the inbound loop must reclaim the entry.
    #[test]
    fn test_server_reclaims_departed_peer_entries() {
        let rt = make_runtime_for_tests();
        let client_keypair = Keypair::new();
        let client_pubkey = client_keypair.pubkey();
        let server = Node::spawn_node(
            &rt,
            Keypair::new(),
            peer_list_with_unknown_addr(client_pubkey),
            HIGH_PPS,
        );
        let client = Node::spawn_node(
            &rt,
            client_keypair,
            peer_list_of(server.pubkey(), server.addr),
            HIGH_PPS,
        );
        let stats = &server.endpoint.server_stats;

        // Establish the connection: the server now holds one inbound peer entry.
        let probe = Bytes::from_static(b"probe");
        send_until_received(&client, &probe, &server.ingress_receiver, |d| {
            (d.peer_pubkey == client_pubkey && d.message == probe).then_some(())
        })
        .expect("server never received the probe");
        assert!(
            stats.peak_unique_peers.load(Ordering::Relaxed) >= 1,
            "the admitted connection must register a unique peer"
        );

        // Close from the client side by dropping the server from its peer_list. The
        // server observes the close (connection_lost) and should close the connection,
        // leaving an empty tombstone entry behind.
        client.set_peer_list(HashMap::new());
        let lost = wait_for_stat(&stats.connection_lost, 1, MAX_IDLE_TIMEOUT * 2);
        assert_eq!(lost, 1, "server must observe the client-initiated close");

        // The background maintenance tick must reclaim the entry once
        // its rate limiter has refilled. peak_unique_peers returns to zero after
        // that reclaim runs.
        let start = Instant::now();

        while stats.peak_unique_peers.load(Ordering::Relaxed) != 0
            && start.elapsed() < METRICS_INTERVAL * 2
        {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(
            stats.peak_unique_peers.load(Ordering::Relaxed),
            0,
            "background maintenance must reclaim the closed peer's entry"
        );
    }

    /// A server with SO_REUSEPORT inbound sockets must accept packets from all of them.
    /// On platforms without SO_REUSEPORT, `bind_more_with_config` yields one socket
    /// and this degrades to a delivery check, but should still pass.
    #[test]
    fn test_server_socket_reuseport_delivers() {
        const NUM_SIBLINGS: usize = 3;
        const NUM_CLIENTS: usize = 6;
        let rt = make_runtime_for_tests();

        let first = bind_to_localhost_unique().expect("bind first inbound socket");
        let inbound_sockets =
            bind_more_with_config(first, NUM_SIBLINGS, SocketConfiguration::default())
                .expect("bind additional reuseport inbound sockets");
        // The kernel hashes each connection's 4-tuple to a socket, but we do not
        // know which one. So we spawn many connections to make sure we cover
        // multiple Endpoints with high probability.
        let client_keypairs: Vec<Keypair> = (0..NUM_CLIENTS).map(|_| Keypair::new()).collect();
        let server_peer_list: HashMap<Pubkey, Option<SocketAddr>> = client_keypairs
            .iter()
            .map(|kp| (kp.pubkey(), None))
            .collect();
        let server = Node::spawn_node_with_sockets(
            &rt,
            Keypair::new(),
            server_peer_list,
            HIGH_PPS,
            inbound_sockets,
        );
        let server_pubkey = server.pubkey();
        let clients: Vec<Node> = client_keypairs
            .into_iter()
            .map(|kp| Node::spawn_node(&rt, kp, peer_list_of(server_pubkey, server.addr), HIGH_PPS))
            .collect();

        // Every client must get a datagram through.
        for (i, client) in clients.iter().enumerate() {
            let client_pubkey = client.pubkey();
            let probe = Bytes::from(format!("reuseport-probe-{i}").into_bytes());
            send_until_received(client, &probe, &server.ingress_receiver, |d| {
                (d.peer_pubkey == client_pubkey && d.message == probe).then_some(())
            })
            .expect("server never received a datagram from one of the clients");
            // Clear this client's retransmits before checking the next one.
            drain_backlog(&server.ingress_receiver);
        }
    }

    /// A live outbound connection whose server disappears must be detected as dead
    /// and reconnected once the server is back.
    #[test]
    fn test_client_reconnects_after_server_restart() {
        let rt = make_runtime_for_tests();
        let server_keypair = Keypair::new();
        let server_pubkey = server_keypair.pubkey();
        let client_keypair = Keypair::new();
        let client_pubkey = client_keypair.pubkey();

        let localhost = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let server_port = unique_port_range_for_tests(1).start;
        let server_addr = SocketAddr::new(localhost, server_port);
        let server_socket = bind_to(localhost, server_port).expect("bind server to reserved port");

        let server = Node::spawn_node_with_sockets(
            &rt,
            server_keypair.insecure_clone(),
            peer_list_with_unknown_addr(client_pubkey),
            HIGH_PPS,
            vec![server_socket],
        );
        let client = Node::spawn_node(
            &rt,
            client_keypair,
            peer_list_of(server_pubkey, server_addr),
            HIGH_PPS,
        );

        let probe = Bytes::from_static(b"before-restart");
        send_until_received(&client, &probe, &server.ingress_receiver, |d| {
            (d.peer_pubkey == client_pubkey && d.message == probe).then_some(())
        })
        .expect("server never received the pre-restart probe");

        // Kill the server and stay silent: with nothing being sent, the client
        // cannot notice the death via a failed `send_datagram`, so the dead
        // connection is reclaimed by reconcile once the idle timeout fires.
        drop(server);
        std::thread::sleep(MAX_IDLE_TIMEOUT + Duration::from_secs(2));

        // Restart on the same port under the same identity.
        let server_socket = bind_to(localhost, server_port).expect("bind server to reserved port");
        let server = Node::spawn_node_with_sockets(
            &rt,
            server_keypair,
            peer_list_with_unknown_addr(client_pubkey),
            HIGH_PPS,
            vec![server_socket],
        );

        let after = Bytes::from_static(b"after-restart");
        send_until_received(&client, &after, &server.ingress_receiver, |d| {
            (d.peer_pubkey == client_pubkey && d.message == after).then_some(())
        })
        .expect("client did not reconnect and resume delivery after server restart");
    }

    /// The client verifies the server's attested identity against the pubkey it
    /// intended to reach.
    #[test]
    fn test_client_rejects_identity_mismatch() {
        let rt = make_runtime_for_tests();
        let client_keypair = Keypair::new();
        let client_pubkey = client_keypair.pubkey();
        let server = Node::spawn_node(
            &rt,
            Keypair::new(),
            peer_list_with_unknown_addr(client_pubkey),
            HIGH_PPS,
        );
        let server_pubkey = server.pubkey();

        // The client's peer_list points at the server's address but expects a
        // different identity there.
        let wrong_pubkey = Keypair::new().pubkey();
        let client = Node::spawn_node(
            &rt,
            client_keypair,
            peer_list_of(wrong_pubkey, server.addr),
            HIGH_PPS,
        );

        let to_impostor = Bytes::from_static(b"to-impostor");
        assert_not_delivered(&client, &to_impostor, &server.ingress_receiver, 20);

        // Retarget the client at the server's true identity: delivery resumes,
        // proving the server was reachable and willing all along, so the identity
        // mismatch was the sole blocker.
        client.set_peer_list(peer_list_of(server_pubkey, server.addr));
        let to_true_identity = Bytes::from_static(b"to-true-identity");
        send_until_received(&client, &to_true_identity, &server.ingress_receiver, |d| {
            (d.peer_pubkey == client_pubkey && d.message == to_true_identity).then_some(())
        })
        .expect("delivery must resume once the client targets the server's true identity");
    }
}
