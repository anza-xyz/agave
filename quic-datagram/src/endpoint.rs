//! QUIC datagram endpoint
use {
    crate::{
        ALPENGLOW_ALPN, EGRESS_CHANNEL_CAP, HANDSHAKE_GLOBAL_BURST, HANDSHAKE_GLOBAL_RATE,
        MAX_ALPENGLOW_VOTE_ACCOUNTS,
        allowlist::Allowlist,
        client::{DialEvent, OutboundLoop},
        error::Error,
        server::{InboundEvent, InboundLoop},
        stats::QuicDatagramStats,
        transport::{IdentitySnapshot, new_client_config, new_server_config},
    },
    bytes::Bytes,
    crossbeam_channel::Sender,
    lru::LruCache,
    quinn::{Endpoint, EndpointConfig, TokioRuntime},
    solana_keypair::{Keypair, Signer},
    solana_net_utils::{banlist::Banlist, token_bucket::TokenBucket},
    solana_pubkey::{Pubkey, PubkeyHasherBuilder},
    solana_tls_utils::{NotifyKeyUpdate, new_dummy_x509_certificate},
    std::{
        collections::HashMap,
        net::{SocketAddr, UdpSocket},
        num::NonZeroUsize,
        sync::Arc,
    },
    tokio::{
        runtime::Handle,
        sync::{mpsc, watch},
    },
    tokio_util::sync::CancellationToken,
};

/// Handle for caller-driven identity rotation. Cloneable and thread-safe.
pub struct KeyUpdater {
    tx: watch::Sender<Option<Arc<IdentitySnapshot>>>,
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

/// Capacity of the task -> control-loop connection-event channel.
const CONN_EVENT_CHANNEL_CAP: usize = MAX_ALPENGLOW_VOTE_ACCOUNTS;

/// Datagram envelope used on both directions of the endpoint.
#[derive(Debug)]
pub struct Datagram {
    pub peer_pubkey: Pubkey,
    pub peer_address: SocketAddr,
    pub message: Bytes,
}

/// Datagram-only QUIC endpoint bound to a UDP socket.
pub struct QuicDatagramEndpoint {
    pub egress: mpsc::Sender<Datagram>,
    /// Handle for rotating the local identity (TLS cert / pubkey).
    pub key_updater: Arc<KeyUpdater>,
    pub server_stats: Arc<QuicDatagramStats>,
    shutdown: CancellationToken,
}

impl QuicDatagramEndpoint {
    /// Construct a datagram-only QUIC endpoint bound to `socket`. Spawns the
    /// unified control loop on `runtime`. Received datagrams flow into
    /// `ingress` via `try_send`; full ingress channel results in a drop
    /// (counted in `datagram_ingress_dropped_channel_full`).
    ///
    /// `allowlist` and `banlist` define admission policy.
    pub fn new(
        runtime: &Handle,
        keypair: &Keypair,
        socket: UdpSocket,
        ingress: Sender<Datagram>,
        allowlist: Arc<dyn Allowlist>,
        banlist: Arc<Banlist<Pubkey>>,
    ) -> Result<Self, Error> {
        let local_pubkey = keypair.pubkey();
        let (cert, key) = new_dummy_x509_certificate(keypair);
        let server_config = new_server_config(cert.clone(), key.clone_key(), ALPENGLOW_ALPN);
        let client_config = new_client_config(cert, key, ALPENGLOW_ALPN);

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

        // Independent stats instances: each loop owns one and reports it under
        // its own datapoint, so the two directions share no atomics.
        let client_stats = Arc::default();
        let server_stats: Arc<QuicDatagramStats> = Arc::default();
        let (egress_tx, egress_rx) = mpsc::channel(EGRESS_CHANNEL_CAP);
        let (out_events_tx, out_events_rx) = mpsc::channel::<DialEvent>(CONN_EVENT_CHANNEL_CAP);
        let (in_events_tx, in_events_rx) = mpsc::channel::<InboundEvent>(CONN_EVENT_CHANNEL_CAP);
        let shutdown = CancellationToken::new();
        let (id_tx, identity_rx) = watch::channel(None);
        let key_updater = Arc::new(KeyUpdater { tx: id_tx });

        // Limits total TLS verification effort spent by all threads
        let handshake_global_limiter = TokenBucket::new(
            HANDSHAKE_GLOBAL_BURST,
            HANDSHAKE_GLOBAL_BURST,
            HANDSHAKE_GLOBAL_RATE,
        );
        const LRU_SIZE: NonZeroUsize = NonZeroUsize::new(2 * MAX_ALPENGLOW_VOTE_ACCOUNTS).unwrap();
        let outbound = OutboundLoop {
            endpoint: endpoint.clone(),
            local_pubkey,
            generation: 0,
            egress_rx,
            banlist: banlist.clone(),
            identity_rx: identity_rx.clone(),
            outgoing: LruCache::with_hasher(LRU_SIZE, PubkeyHasherBuilder::default()),
            events_tx: out_events_tx,
            events_rx: out_events_rx,
            shutdown: shutdown.clone(),
            stats: client_stats,
        };
        runtime.spawn(outbound.run());
        let inbound = InboundLoop {
            endpoint: endpoint.clone(),
            generation: 0,
            ingress,
            banlist,
            allowlist,
            identity_rx,
            incoming: HashMap::with_hasher(PubkeyHasherBuilder::default()),
            events_tx: in_events_tx,
            events_rx: in_events_rx,
            handshake_global_limiter,
            stats: server_stats.clone(),
            shutdown: shutdown.clone(),
        };
        runtime.spawn(inbound.run());

        Ok(Self {
            egress: egress_tx,
            key_updater,
            server_stats,
            shutdown,
        })
    }

    /// Initiate endpoint shutdown.
    pub fn close(&self) {
        self.shutdown.cancel();
    }
}

#[cfg(test)]
mod tests {
    use {
        crate::{
            allowlist::AllowAll,
            endpoint::Datagram,
            testutils::{drain_matching, make_runtime, send_until_received, spawn_node},
        },
        bytes::Bytes,
        solana_keypair::{Keypair, Signer},
        solana_tls_utils::NotifyKeyUpdate,
        std::{sync::Arc, time::Duration},
    };

    #[test]
    /// Make sure packets flow in both directions
    fn bidirectional_delivery_works() {
        let rt = make_runtime();
        let a = spawn_node(&rt, Arc::new(AllowAll), Keypair::new());
        let b = spawn_node(&rt, Arc::new(AllowAll), Keypair::new());

        // A → B: A dials its outbound to B; B receives on its inbound from A.
        let from_a = Bytes::from_static(b"from-A");
        send_until_received(
            &rt,
            &a.endpoint,
            b.pubkey(),
            b.addr,
            from_a.clone(),
            &b.ingress_rx,
            Duration::from_secs(5),
            |d| (d.peer_pubkey == a.pubkey() && d.message == from_a).then_some(()),
            "B did not receive A's datagram",
        );
        drain_matching(&b.ingress_rx, Duration::from_millis(200), |d| {
            d.message == from_a
        });

        // B → A: a *separate* connection (B's own outbound) carries this - B
        // does not reuse the inbound A dialed. A receives on its inbound from B.
        let from_b = Bytes::from_static(b"from-B");
        send_until_received(
            &rt,
            &b.endpoint,
            a.pubkey(),
            a.addr,
            from_b.clone(),
            &a.ingress_rx,
            Duration::from_secs(5),
            |d| (d.peer_pubkey == b.pubkey() && d.message == from_b).then_some(()),
            "A did not receive B's datagram",
        );
    }

    #[test]
    fn test_pubkey_rotation_evicts_connections_and_resends_under_new_identity() {
        let rt = make_runtime();
        let server = spawn_node(&rt, Arc::new(AllowAll), Keypair::new());

        let k1 = Keypair::new();
        let k1_pk = k1.pubkey();
        let client = spawn_node(&rt, Arc::new(AllowAll), k1);

        // Send under K1. Server should observe message attributed to K1.
        let packet1 = Bytes::from_static(b"under-K1");
        send_until_received(
            &rt,
            &client.endpoint,
            server.pubkey(),
            server.addr,
            packet1.clone(),
            &server.ingress_rx,
            Duration::from_secs(5),
            |d| (d.peer_pubkey == k1_pk && d.message == packet1).then_some(()),
            "server never received message attributed to K1",
        );
        drain_matching(&server.ingress_rx, Duration::from_millis(200), |d| {
            d.message == packet1
        });

        // Rotate to K2.
        let k2 = Keypair::new();
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
        let packet2 = Bytes::from_static(b"under-K2");
        send_until_received(
            &rt,
            &client.endpoint,
            server.pubkey(),
            server.addr,
            packet2.clone(),
            &server.ingress_rx,
            Duration::from_secs(5),
            |d| (d.peer_pubkey == k2_pk && d.message == packet2).then_some(()),
            "server never received message attributed to K2 after rotation",
        );

        // Client side: the rotated client should NOT have banned the server
        // (rotation is a local event; we never soft-ban peers we close
        // ourselves, and there is no handover signal to react to).
        assert!(
            !client.banlist.is_banned(&server.pubkey()),
            "rotation must not ban peers we close ourselves"
        );
    }

    #[test]
    /// Rotating the SERVER's identity must evict its inbound table: the live
    /// connection authenticated under the old server identity is closed
    /// (IDENTITY_ROTATED), so datagrams targeting the old server pubkey stop
    /// being delivered, while the new server identity is reachable at the same
    /// address.
    fn server_identity_rotation_evicts_inbound() {
        let rt = make_runtime();
        let server = spawn_node(&rt, Arc::new(AllowAll), Keypair::new());
        let s_pk1 = server.pubkey();
        let client = spawn_node(&rt, Arc::new(AllowAll), Keypair::new());

        // Establish + deliver under the server's first identity.
        let packet1 = Bytes::from_static(b"under-server-id-1");
        send_until_received(
            &rt,
            &client.endpoint,
            s_pk1,
            server.addr,
            packet1.clone(),
            &server.ingress_rx,
            Duration::from_secs(5),
            |d| (d.message == packet1).then_some(()),
            "server never received datagram under identity 1",
        );
        drain_matching(&server.ingress_rx, Duration::from_millis(200), |d| {
            d.message == packet1
        });

        // Rotate the server's identity.
        let server_k2 = Keypair::new();
        let s_pk2 = server_k2.pubkey();
        assert_ne!(s_pk1, s_pk2, "server identities must differ");
        server
            .endpoint
            .key_updater
            .update_key(&server_k2)
            .expect("server identity rotation accepted");

        // Let the inbound loop rebuild its TLS config and evict the table.
        std::thread::sleep(Duration::from_millis(500));

        // Without eviction the established QUIC connection would persist and keep
        // delivering. Datagrams targeting the OLD server pubkey must no longer
        // arrive: the old connection is closed, and re-dials now attest s_pk2 and
        // are rejected client-side with INVALID_IDENTITY.
        let stale = Bytes::from_static(b"to-old-server-id");
        for _ in 0..20 {
            let _ = client.endpoint.egress.try_send(Datagram {
                peer_pubkey: s_pk1,
                peer_address: server.addr,
                message: stale.clone(),
            });
            if let Ok(d) = server.ingress_rx.recv_timeout(Duration::from_millis(100)) {
                assert_ne!(
                    d.message, stale,
                    "server inbound under the rotated-away identity must be evicted"
                );
            }
        }

        // The new identity is reachable at the same addr.
        let packet2 = Bytes::from_static(b"under-server-id-2");
        send_until_received(
            &rt,
            &client.endpoint,
            s_pk2,
            server.addr,
            packet2.clone(),
            &server.ingress_rx,
            Duration::from_secs(5),
            |d| (d.message == packet2).then_some(()),
            "server never received datagram under rotated identity 2",
        );
    }

    #[test]
    fn outbound_addr_change_redials_new_addr() {
        let rt = make_runtime();

        let s1 = spawn_node(&rt, Arc::new(AllowAll), Keypair::new());
        let s_key = s1.keypair.insecure_clone();
        let s_pubkey = s1.pubkey();

        let client = spawn_node(&rt, Arc::new(AllowAll), Keypair::new());

        // Initial send: client dials S1 at its addr A1.
        let packet1 = Bytes::from_static(b"p1");
        send_until_received(
            &rt,
            &client.endpoint,
            s_pubkey,
            s1.addr,
            packet1.clone(),
            &s1.ingress_rx,
            Duration::from_secs(5),
            |d| (d.message == packet1).then_some(()),
            "S1 did not receive packet1",
        );
        drain_matching(&s1.ingress_rx, Duration::from_millis(200), |d| {
            d.message == packet1
        });

        // S2 takes the same identity at a new addr - simulates a peer host
        // move with gossip publishing a new SocketAddr for the same pubkey.
        let s2 = spawn_node(&rt, Arc::new(AllowAll), s_key);
        assert_ne!(s1.addr, s2.addr, "S1 and S2 must bind distinct addrs");

        // Send to the new addr. Client must observe the addr mismatch,
        // evict its cached conn to A1, and re-dial A2.
        let packet2 = Bytes::from_static(b"p2");
        send_until_received(
            &rt,
            &client.endpoint,
            s_pubkey,
            s2.addr,
            packet2.clone(),
            &s2.ingress_rx,
            Duration::from_secs(5),
            |d| (d.message == packet2).then_some(()),
            "S2 (post-move) did not receive packet2",
        );

        // Stale conn to S1 was evicted; S1 must not see post-move datagrams.
        let stray = s1.ingress_rx.recv_timeout(Duration::from_millis(800));
        assert!(
            stray.is_err(),
            "S1 must not see post-move datagrams; got {stray:?}"
        );
    }
}
