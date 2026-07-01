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
    pub message: Bytes,
}

/// Datagram-only QUIC endpoint.
pub struct QuicDatagramEndpoint {
    /// Egress is broadcast: one queued message is fanned out to every live
    /// outbound connection.
    pub egress: mpsc::Sender<Bytes>,
    /// Handle for rotating the local identity (TLS cert / pubkey).
    pub key_updater: Arc<KeyUpdater>,
    /// A retained `peer_list` receiver clone. Exists so a peer_list update
    /// never panics just because the loop tasks have exited during teardown.
    _peer_list: PeerListReceiver,
    exit_signals: ExitSignals,
    /// Inbound stats, exposed so integration tests can assert on them.
    #[cfg(any(test, feature = "dev-context-only-utils"))]
    pub server_stats: Arc<ServerStats>,
    /// Spawned event-loop tasks. In test / DCOU builds `Drop` joins these so a panic
    /// that tokio would otherwise swallow becomes a real failure.
    task_handles: JoinSet<()>,
    #[cfg(any(test, feature = "dev-context-only-utils"))]
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
    pub fn spawn(
        runtime: &Handle,
        keypair: &Keypair,
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
            server_stats,
            exit_signals.clone(),
            max_datagrams_per_second_per_peer,
        );
        task_handles.spawn_on(inbound.run(), runtime);

        Ok(Self {
            egress: egress_sender,
            key_updater,
            _peer_list: peer_list,
            exit_signals,
            task_handles,
            #[cfg(any(test, feature = "dev-context-only-utils"))]
            runtime_handle: runtime.clone(),
            #[cfg(any(test, feature = "dev-context-only-utils"))]
            server_stats: endpoint_server_stats,
        })
    }
}

impl Drop for QuicDatagramEndpoint {
    /// Cancel the spawned loops so a dropped handle can't leak them.
    fn drop(&mut self) {
        self.exit_signals.cancel();
        #[cfg(any(test, feature = "dev-context-only-utils"))]
        {
            // Join all the internal tasks so a panic that a loop task
            // would otherwise swallow turns into a real test failure instead of a
            // green run with a stray "task panicked" line on stderr.
            let errors = self.runtime_handle.block_on(async {
                let mut errors = vec![];
                loop {
                    match tokio::time::timeout(
                        Duration::from_secs(1),
                        self.task_handles.join_next(),
                    )
                    .await
                    {
                        Ok(Some(Ok(()))) => {}
                        Ok(Some(Err(join_err))) => {
                            if join_err.is_panic() {
                                errors.push(format!(
                                    "QuicDatagramEndpoint teardown error {join_err}"
                                ));
                            }
                        }
                        // All loop tasks have exited.
                        Ok(None) => break,
                        // Stuck tasks on shutdown are also a concern.
                        Err(_elapsed) => {
                            errors.push(
                                "QuicDatagramEndpoint teardown: a loop task did not exit within 1s"
                                    .to_string(),
                            );
                        }
                    }
                }
                errors
            });
            if !errors.is_empty() {
                // Guard against a double panic (which aborts the process).
                if !std::thread::panicking() {
                    panic!("QuicDatagramEndpoint encountered errors: {errors:?}");
                } else {
                    eprintln!("QuicDatagramEndpoint encountered errors: {errors:?}");
                }
            }
        }
        // Detach rather than abort: in prod the loops wind down on their own once
        // `shutdown` fires; in test/dev the set was already drained above.
        self.task_handles.detach_all();
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
            ADDRESS_UNKNOWN, ALPENGLOW_ALPN, HANDSHAKE_BURST, HANDSHAKE_GLOBAL_RATE,
            MAX_ALPENGLOW_VOTE_ACCOUNTS, MAX_INFLIGHT_HANDSHAKES, METRICS_INTERVAL, PeerListSender,
            transport::{Identity, MAX_IDLE_TIMEOUT, new_client_config},
        },
        bytes::Bytes,
        crossbeam_channel::{Receiver, bounded},
        quinn_proto::{Endpoint as ProtoEndpoint, EndpointConfig as ProtoEndpointConfig},
        solana_keypair::{Keypair, Signer},
        solana_net_utils::sockets::{
            SocketConfiguration, bind_more_with_config, bind_to, bind_to_localhost_unique,
            unique_port_range_for_tests,
        },
        solana_pubkey::Pubkey,
        solana_tls_utils::{NotifyKeyUpdate, socket_addr_to_quic_server_name},
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

    fn spawn_node(
        rt: &Runtime,
        keypair: Keypair,
        peer_list: HashMap<Pubkey, SocketAddr>,
        max_pps: usize,
    ) -> Node {
        let server_socket = bind_to_localhost_unique().expect("bind server UDP");
        spawn_node_with_sockets(rt, keypair, peer_list, max_pps, vec![server_socket])
    }

    /// As [`spawn_node`], but the caller supplies the inbound socket(s). Used to
    /// drive the multi-endpoint (SO_REUSEPORT) path where `inbound_sockets.len() > 1`.
    fn spawn_node_with_sockets(
        rt: &Runtime,
        keypair: Keypair,
        peer_list: HashMap<Pubkey, SocketAddr>,
        max_pps: usize,
        inbound_sockets: Vec<UdpSocket>,
    ) -> Node {
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
            inbound_sockets,
            client_socket,
            ingress_sender,
            peer_list_receiver,
            ban_receiver,
            max_pps,
            ExitSignals::new(Arc::new(AtomicBool::new(false)), CancellationToken::new()),
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

    /// Drive a quinn-proto endpoint to emit fresh Initials at `server_addr` as fast
    /// as the thread can, never advancing any handshake, until `stop` is set. Each
    /// connection is forgotten after its Initial, so the server never receives a
    /// reply and keeps every accepted handshake in-flight until it times out.
    ///
    /// This does actively leak quinn state, so running this for > 5 seconds is
    /// not a good idea.
    fn flood_initials(
        stop: &AtomicBool,
        client_config: &quinn_proto::ClientConfig,
        server_addr: SocketAddr,
        server_name: &str,
    ) {
        let udp = bind_to_localhost_unique().expect("bind flood socket");
        let mut buf = Vec::with_capacity(1300);
        let mut proto =
            ProtoEndpoint::new(Arc::new(ProtoEndpointConfig::default()), None, false, None);
        while !stop.load(Ordering::Relaxed) {
            let now = Instant::now();
            let Ok((_ch, mut conn)) =
                proto.connect(now, client_config.clone(), server_addr, server_name)
            else {
                break;
            };
            buf.clear();
            if let Some(transmit) = conn.poll_transmit(now, 1, &mut buf) {
                let _ = udp.send_to(&buf[..transmit.size], transmit.destination);
            }
        }
    }

    /// Make sure client keeps trying to connect.
    #[test]
    fn test_client_keeps_trying() {
        let rt = make_runtime_for_tests();
        let a_kp = Keypair::new();
        let b_kp = Keypair::new();
        let a_pk = a_kp.pubkey();
        let b_pk = b_kp.pubkey();
        let a = spawn_node(&rt, a_kp, HashMap::new(), HIGH_PPS);
        let b = spawn_node(&rt, b_kp, HashMap::new(), HIGH_PPS);
        a.set_peer_list(peer_list_of(b_pk, b.addr));

        let from_a = Bytes::from_static(b"from-A");
        send_until_received(&a, &from_a, &b.ingress_receiver, |d| {
            (d.peer_pubkey == a_pk && d.message == from_a).then_some(())
        })
        .expect_err("B should not have received anything");
        // now allow A to connect
        b.set_peer_list(peer_list_of(a_pk, a.addr));
        send_until_received(&a, &from_a, &b.ingress_receiver, |d| {
            (d.peer_pubkey == a_pk && d.message == from_a).then_some(())
        })
        .expect("B never received A's datagram");
    }
    /// Basic exchange: each node lists the other in its peer_list, datagrams flow
    /// in both directions over two independent send-only connections.
    #[test]
    fn test_delivery_flows_both_directions() {
        let rt = make_runtime_for_tests();
        let a_kp = Keypair::new();
        let b_kp = Keypair::new();
        let a_pk = a_kp.pubkey();
        let b_pk = b_kp.pubkey();
        let a = spawn_node(&rt, a_kp, HashMap::new(), HIGH_PPS);
        let b = spawn_node(&rt, b_kp, HashMap::new(), HIGH_PPS);
        a.set_peer_list(peer_list_of(b_pk, b.addr));
        b.set_peer_list(peer_list_of(a_pk, a.addr));

        let from_a = Bytes::from_static(b"from-A");
        send_until_received(&a, &from_a, &b.ingress_receiver, |d| {
            (d.peer_pubkey == a_pk && d.message == from_a).then_some(())
        })
        .expect("B never received A's datagram");
        let from_b = Bytes::from_static(b"from-B");
        send_until_received(&b, &from_b, &a.ingress_receiver, |d| {
            (d.peer_pubkey == b_pk && d.message == from_b).then_some(())
        })
        .expect("A never received B's datagram");
    }

    /// A peer in the server's peer_list is admitted; one that is not is rejected
    /// at admission (NOT_ADMITTED) and never reaches ingress.
    #[test]
    fn test_server_admitted_peer_delivers_unadmitted_is_rejected() {
        let rt = make_runtime_for_tests();
        let a_kp = Keypair::new();
        let a_pk = a_kp.pubkey();
        let server = spawn_node(
            &rt,
            Keypair::new(),
            peer_list_of(a_pk, ADDRESS_UNKNOWN),
            HIGH_PPS,
        );
        let server_pk = server.pubkey();
        let client_a = spawn_node(&rt, a_kp, peer_list_of(server_pk, server.addr), HIGH_PPS);
        let client_b = spawn_node(
            &rt,
            Keypair::new(),
            peer_list_of(server_pk, server.addr),
            HIGH_PPS,
        );

        let payload_a = Bytes::from_static(b"hello-from-A");
        send_until_received(&client_a, &payload_a, &server.ingress_receiver, |d| {
            (d.peer_pubkey == a_pk && d.message == payload_a).then_some(())
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

    /// Banning a peer closes its live connection and
    /// blocks subsequent connections.
    #[test]
    fn test_server_ban_evicts_existing_and_blocks_handshake() {
        let rt = make_runtime_for_tests();
        let client_keypair = Keypair::new();
        let client_pubkey = client_keypair.pubkey();
        let server = spawn_node(
            &rt,
            Keypair::new(),
            peer_list_of(client_pubkey, ADDRESS_UNKNOWN),
            HIGH_PPS,
        );
        let client = spawn_node(
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

    /// Two client instances sharing one identity each bring up a *separate*
    /// inbound connection, a third same-identity instance is refused.
    #[test]
    fn test_server_two_inbound_same_identity_coexist_third_refused() {
        let rt = make_runtime_for_tests();
        let shared = Keypair::new();
        let shared_pk = shared.pubkey();
        let server = spawn_node(
            &rt,
            Keypair::new(),
            peer_list_of(shared_pk, ADDRESS_UNKNOWN),
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
        let c1 = spawn_node(&rt, shared.insecure_clone(), peers.clone(), HIGH_PPS);
        let c2 = spawn_node(&rt, shared.insecure_clone(), peers.clone(), HIGH_PPS);

        let p1 = Bytes::from_static(b"from-c1");
        send_until_received(&c1, &p1, &server.ingress_receiver, |d| {
            (d.message == p1).then_some(())
        })
        .expect("server did not receive c1's probe");
        drain_backlog(&server.ingress_receiver);
        let p2 = Bytes::from_static(b"from-c2");
        send_until_received(&c2, &p2, &server.ingress_receiver, |d| {
            (d.message == p2).then_some(())
        })
        .expect("server did not receive c2's probe (second same-identity inbound)");
        drain_backlog(&server.ingress_receiver);

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
    fn test_server_peer_list_eviction_closes_live_connections() {
        let rt = make_runtime_for_tests();
        // Two instances of one identity give the server two inbound connections.
        let shared = Keypair::new();
        let shared_pk = shared.pubkey();
        let server = spawn_node(
            &rt,
            Keypair::new(),
            peer_list_of(shared_pk, ADDRESS_UNKNOWN),
            HIGH_PPS,
        );
        let peers = peer_list_of(server.pubkey(), server.addr);
        let c1 = spawn_node(&rt, shared.insecure_clone(), peers.clone(), HIGH_PPS);
        let c2 = spawn_node(&rt, shared.insecure_clone(), peers.clone(), HIGH_PPS);

        let p1 = Bytes::from_static(b"from-c1");
        send_until_received(&c1, &p1, &server.ingress_receiver, |d| {
            (d.message == p1).then_some(())
        })
        .expect("server never received c1's probe");
        drain_backlog(&server.ingress_receiver);
        let p2 = Bytes::from_static(b"from-c2");
        send_until_received(&c2, &p2, &server.ingress_receiver, |d| {
            (d.message == p2).then_some(())
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
        assert_not_delivered(&c1, &after, &server.ingress_receiver, 20);
        assert_not_delivered(&c2, &after, &server.ingress_receiver, 20);
    }

    /// Per-connection rate-limit check: a burst above the refill rate is
    /// dropped and the connection survives. a sustained flood that drains
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
        let server = spawn_node(
            &rt,
            Keypair::new(),
            peer_list_of(client_pubkey, ADDRESS_UNKNOWN),
            PPS,
        );
        // Keep `datagram_rate_limited` / `connection_lost` cumulative across report ticks.
        server
            .endpoint
            .server_stats
            .report_frozen
            .store(true, Ordering::Relaxed);
        let client = spawn_node(
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

    /// Rotating the CLIENT's identity evicts its outbound connections and
    /// reconnects under the new key, the server observes the post-rotation
    /// messages attributed to the new pubkey.
    #[test]
    fn test_client_identity_rotation_resends_under_new_identity() {
        let rt = make_runtime_for_tests();
        let k1 = Keypair::new();
        let k1_pk = k1.pubkey();
        let k2 = Keypair::new();
        let k2_pk = k2.pubkey();
        // Server admits both the old and new client identities so the rotated
        // client is still accepted after re-handshaking under K2.
        let server = spawn_node(
            &rt,
            Keypair::new(),
            HashMap::from([(k1_pk, ADDRESS_UNKNOWN), (k2_pk, ADDRESS_UNKNOWN)]),
            HIGH_PPS,
        );
        let client = spawn_node(
            &rt,
            k1,
            peer_list_of(server.pubkey(), server.addr),
            HIGH_PPS,
        );

        let p1 = Bytes::from_static(b"under-K1");
        send_until_received(&client, &p1, &server.ingress_receiver, |d| {
            (d.peer_pubkey == k1_pk && d.message == p1).then_some(())
        })
        .expect("server never received message attributed to K1");
        drain_backlog(&server.ingress_receiver);

        client
            .endpoint
            .key_updater
            .update_key(&k2)
            .expect("identity rotation accepted");
        std::thread::sleep(Duration::from_millis(500));

        let p2 = Bytes::from_static(b"under-K2");
        send_until_received(&client, &p2, &server.ingress_receiver, |d| {
            (d.peer_pubkey == k2_pk && d.message == p2).then_some(())
        })
        .expect("server never received message attributed to K2 after rotation");
    }

    /// Rotating the SERVER's identity closes every inbound connection that was
    /// accepted under the old identity (close code IDENTITY_CHANGED).
    #[test]
    fn test_server_identity_rotation_evicts_inbound() {
        let rt = make_runtime_for_tests();
        let client_keypair = Keypair::new();
        let client_pubkey = client_keypair.pubkey();
        let server = spawn_node(
            &rt,
            Keypair::new(),
            peer_list_of(client_pubkey, ADDRESS_UNKNOWN),
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
        send_until_received(&client, &p1, &server.ingress_receiver, |d| {
            (d.message == p1).then_some(())
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
        let p2 = Bytes::from_static(b"under-server-id-2");
        send_until_received(&client, &p2, &server.ingress_receiver, |d| {
            (d.message == p2).then_some(())
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
        let server1 = spawn_node(
            &rt,
            server_keypair.insecure_clone(),
            peer_list_of(client_pubkey, ADDRESS_UNKNOWN),
            HIGH_PPS,
        );
        let client = spawn_node(
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
        let server2 = spawn_node(
            &rt,
            server_keypair.insecure_clone(),
            peer_list_of(client_pubkey, ADDRESS_UNKNOWN),
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

    /// A node should never connect to itself.
    #[test]
    fn test_client_connections_excludes_self() {
        let rt = make_runtime_for_tests();
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

    /// Background GC: after an inbound connection closes, the peer entry lingers as
    /// a tombstone (the per-peer rate limiter is retained in case the peer reconnects
    /// quickly). Once that limiter refills, the inbound loop must reclaim the entry.
    #[test]
    fn test_server_reclaims_departed_peer_entries() {
        let rt = make_runtime_for_tests();
        let client_keypair = Keypair::new();
        let client_pubkey = client_keypair.pubkey();
        let server = spawn_node(
            &rt,
            Keypair::new(),
            peer_list_of(client_pubkey, ADDRESS_UNKNOWN),
            HIGH_PPS,
        );
        let client = spawn_node(
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

    /// A server bound with several inbound sockets on one port (SO_REUSEPORT) must
    /// accept packets from all of them. The kernel hashes each connection's 4-tuple to
    /// a socket, but we do not know which one. So we spawn many connections to make sure
    /// we cover multiple Endpoints with high probability.
    ///
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

        let client_keypairs: Vec<Keypair> = (0..NUM_CLIENTS).map(|_| Keypair::new()).collect();
        let server_peer_list: HashMap<Pubkey, SocketAddr> = client_keypairs
            .iter()
            .map(|kp| (kp.pubkey(), ADDRESS_UNKNOWN))
            .collect();
        let server = spawn_node_with_sockets(
            &rt,
            Keypair::new(),
            server_peer_list,
            HIGH_PPS,
            inbound_sockets,
        );
        let server_pubkey = server.pubkey();
        let clients: Vec<Node> = client_keypairs
            .into_iter()
            .map(|kp| spawn_node(&rt, kp, peer_list_of(server_pubkey, server.addr), HIGH_PPS))
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

    /// Inbound handshakes that never complete must not accumulate past
    /// [`MAX_INFLIGHT_HANDSHAKES`]. We drive the QUIC state machine by hand:
    /// send each connection's Initial but never reply to the server's handshake,
    /// so every accepted handshake stays in-flight until it times out.
    #[ignore = "Unreliable under codecov"]
    #[test]
    fn test_server_inflight_handshakes_are_capped() {
        let rt = make_runtime_for_tests();
        let node = spawn_node(&rt, Keypair::new(), HashMap::new(), HIGH_PPS);
        let server_addr = node.addr;
        let server_name = socket_addr_to_quic_server_name(server_addr);

        // Any identity works: the rate limit and in-flight cap are enforced
        // before peer admission, so these connections never need to be admitted.
        let Identity { cert, key, .. } = Identity::from_keypair(&Keypair::new());
        let client_config = new_client_config(cert, key, ALPENGLOW_ALPN);

        let stop = AtomicBool::new(false);
        let stats = &node.endpoint.server_stats;
        // Keep counters cumulative across report ticks so started−timed_out reflects
        // true in-flight; otherwise report_server resets them every METRICS_INTERVAL.
        stats.report_frozen.store(true, Ordering::Relaxed);
        let mut peak_inflight = 0usize;
        std::thread::scope(|s| {
            s.spawn(|| flood_initials(&stop, &client_config, server_addr, &server_name));

            let start = Instant::now();
            while start.elapsed() < Duration::from_millis(2000) {
                // The flood never answers, so a started handshake can only leave the in-flight
                // set by timing out.
                let started = stats.handshakes_started.load(Ordering::Relaxed);
                let done = stats.handshake_timed_out.load(Ordering::Relaxed);
                let inflight = started.saturating_sub(done) as usize;
                peak_inflight = peak_inflight.max(inflight);
                std::thread::sleep(Duration::from_millis(20));
            }
            stop.store(true, Ordering::Relaxed);
        });

        // The cap must never be exceeded, and should be approached (proving the flood
        // stressed it). Checking the peak covers every sample at once.
        assert!(
            peak_inflight <= MAX_INFLIGHT_HANDSHAKES,
            "in-flight handshakes {peak_inflight} exceeded the cap {MAX_INFLIGHT_HANDSHAKES}"
        );
        assert!(
            peak_inflight >= MAX_INFLIGHT_HANDSHAKES * 5 / 6,
            "in-flight handshakes should approach the cap; peak={peak_inflight}, \
             cap={MAX_INFLIGHT_HANDSHAKES}"
        );
    }

    /// The handshake rate limiter caps how fast inbound handshakes are *started*.
    /// Its contract is a token bucket: the cumulative number started can never
    /// exceed `HANDSHAKE_BURST + HANDSHAKE_GLOBAL_RATE * elapsed`. We saturate it
    /// with Initials and assert that ceiling holds.
    #[test]
    fn test_server_handshake_rate_is_limited() {
        let rt = make_runtime_for_tests();
        let node = spawn_node(&rt, Keypair::new(), HashMap::new(), HIGH_PPS);
        let server_addr = node.addr;
        let server_name = socket_addr_to_quic_server_name(server_addr);

        let Identity { cert, key, .. } = Identity::from_keypair(&Keypair::new());
        let client_config = new_client_config(cert, key, ALPENGLOW_ALPN);

        let stop = AtomicBool::new(false);
        let stats = &node.endpoint.server_stats;
        let (started, elapsed) = std::thread::scope(|s| {
            // Time from just before the flood starts: any delay before the first
            // Initial only makes the ceiling more generous, never tighter.
            let t0 = Instant::now();
            s.spawn(|| flood_initials(&stop, &client_config, server_addr, &server_name));

            // The in-flight cap would become the binding constraint once `started`
            // reaches MAX_INFLIGHT_HANDSHAKES, which (nothing times out before
            // HANDSHAKE_TIMEOUT) happens after this many seconds:
            //   (MAX_INFLIGHT_HANDSHAKES - HANDSHAKE_BURST) / HANDSHAKE_GLOBAL_RATE.
            // Measure over half of that headroom so the count stays well clear of it.
            let secs_until_cap_binds =
                (MAX_INFLIGHT_HANDSHAKES as f64 - HANDSHAKE_BURST as f64) / HANDSHAKE_GLOBAL_RATE;
            std::thread::sleep(Duration::from_secs_f64(secs_until_cap_binds / 2.0));
            let started = stats.handshakes_started.load(Ordering::Relaxed);
            // Read elapsed *after* the count, so it spans at least as long as the count
            // accrued over, keeping the ceiling on the safe side.
            let elapsed = t0.elapsed().as_secs_f64();
            stop.store(true, Ordering::Relaxed);
            (started, elapsed)
        });

        assert!(
            started < MAX_INFLIGHT_HANDSHAKES as u64,
            "in-flight cap bound during the measurement, invalidating the rate check: \
             started={started}"
        );
        // We assert only the upper bound, not a target rate: the achieved rate depends
        // on the optimized build and platform hardware. The bound must hold always.
        // 1.1 absorbs measurement jitter and token-bucket granularity.
        let ceiling = (HANDSHAKE_BURST as f64 + HANDSHAKE_GLOBAL_RATE * elapsed) * 1.1;
        assert!(
            (started as f64) <= ceiling,
            "started {started} handshakes in {elapsed:.3}s, exceeding the token-bucket ceiling \
             {ceiling:.0} (burst {HANDSHAKE_BURST} + {HANDSHAKE_GLOBAL_RATE}/s)"
        );
    }

    /// A live outbound connection whose server disappears must be detected as dead
    /// and reconnected once the server is back. Exercises the reconcile reconnect
    /// path: with no sends in flight, the death is noticed by the idle timeout
    /// rather than by a failed `send_datagram`.
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

        let server = spawn_node_with_sockets(
            &rt,
            server_keypair.insecure_clone(),
            peer_list_of(client_pubkey, ADDRESS_UNKNOWN),
            HIGH_PPS,
            vec![server_socket],
        );
        let client = spawn_node(
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
        let server = spawn_node_with_sockets(
            &rt,
            server_keypair,
            peer_list_of(client_pubkey, ADDRESS_UNKNOWN),
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
    /// intended to reach. Pointed at a real server under the wrong pubkey, it
    /// completes the TLS handshake, but rejects the connection so it delivers nothing.
    #[test]
    fn test_client_rejects_identity_mismatch() {
        let rt = make_runtime_for_tests();
        let client_keypair = Keypair::new();
        let client_pubkey = client_keypair.pubkey();
        let server = spawn_node(
            &rt,
            Keypair::new(),
            peer_list_of(client_pubkey, ADDRESS_UNKNOWN),
            HIGH_PPS,
        );
        let server_pubkey = server.pubkey();

        // The client's peer_list points at the server's address but expects a
        // different identity there.
        let wrong_pubkey = Keypair::new().pubkey();
        let client = spawn_node(
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
