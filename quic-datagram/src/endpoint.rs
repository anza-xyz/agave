//! QUIC datagram endpoint
use {
    crate::{
        ALPENGLOW_ALPN, CONN_EVENT_CHANNEL_CAP, PeerlistReceiver,
        client::OutboundLoop,
        error::Error,
        server::{AcceptLoop, InboundEvent, InboundLoop},
        stats::{ClientStats, ServerStats},
        transport::{IdentitySnapshot, new_client_config, new_server_config},
    },
    bytes::Bytes,
    crossbeam_channel::Sender,
    quinn::{Endpoint, EndpointConfig, TokioRuntime},
    solana_keypair::{Keypair, Signer},
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

/// Command to temporarily ban a peer, sent from the sig-verifier to the
/// endpoint over a channel. The endpoint owns the banlist; this channel is the
/// only way to mutate it.
pub type BanCommand = (Pubkey, Duration);

/// Handle for caller-driven identity rotation. Cloneable and thread-safe.
pub struct KeyUpdater {
    sender: watch::Sender<Option<Arc<IdentitySnapshot>>>,
}

impl NotifyKeyUpdate for KeyUpdater {
    fn update_key(&self, keypair: &Keypair) -> Result<(), Box<dyn std::error::Error>> {
        let snap = Arc::new(IdentitySnapshot::from_keypair(keypair));
        self.sender
            .send(Some(snap))
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

/// Datagram-only QUIC endpoint bound to a UDP socket.
pub struct QuicDatagramEndpoint {
    /// Egress is broadcast: one queued message is fanned out to every live
    /// outbound connection by the loop. The caller does not address peers.
    pub egress: mpsc::Sender<Bytes>,
    /// Handle for rotating the local identity (TLS cert / pubkey).
    pub key_updater: Arc<KeyUpdater>,
    pub server_stats: Arc<ServerStats>,
    shutdown: CancellationToken,
}

impl QuicDatagramEndpoint {
    /// Construct a datagram-only QUIC endpoint. `server_sockets` back the
    /// inbound (we-accept) direction and are expected to be SO_REUSEPORT
    /// siblings bound to the same address to load-balance inbound datagrams
    /// across them. The outbound (send-only) direction runs on a
    /// dedicated `client_socket` bound to its own port — NOT a member of the
    /// SO_REUSEPORT accept group. A client Endpoint sharing the group's port
    /// would have peers' handshake replies load-balanced by the kernel across
    /// the other sockets, making connections time out.
    ///
    /// Spawns the inbound and outbound loops on `runtime`; dropping the handle
    /// cancels them. Received datagrams flow into `ingress` via `try_send`;
    /// full ingress channel results in a drop
    /// (counted in `datagram_ingress_dropped_channel_full`).
    ///
    /// `peerlist_receiver` carries whole-set peerlist updates: inbound evicts
    /// peers no longer in the set, outbound connects to peers in it.
    /// `ban_receiver` carries temporary per-peer ban commands (each force-closes
    /// the peer's open connections). `peerlist_receiver` is `None` only in
    /// dev/test builds, where inbound admits all peers and outbound connects to
    /// nobody.
    pub fn spawn(
        runtime: &Handle,
        keypair: &Keypair,
        server_sockets: Vec<UdpSocket>,
        client_socket: UdpSocket,
        ingress: Sender<Datagram>,
        peerlist_receiver: Option<PeerlistReceiver>,
        ban_receiver: mpsc::Receiver<BanCommand>,
        max_datagrams_per_second_per_peer: f64,
    ) -> Result<Self, Error> {
        // A release build must gate inbound admission on a real peerlist;
        // `None` (admit-all) is only legitimate in dev/test builds.
        assert!(
            cfg!(feature = "dev-context-only-utils") || peerlist_receiver.is_some(),
            "peerlist receiver must be set in release builds",
        );
        if server_sockets.is_empty() {
            return Err(Error::NoSockets);
        }
        let local_pubkey = keypair.pubkey();
        let (cert, key) = new_dummy_x509_certificate(keypair);
        let server_config = new_server_config(cert.clone(), key.clone_key(), ALPENGLOW_ALPN);
        let client_config = new_client_config(cert, key, ALPENGLOW_ALPN);

        // One quinn endpoint per SO_REUSEPORT socket. All carry the server
        // config so any of them can accept; the kernel decides which socket a
        // given inbound 4-tuple lands on.
        let (endpoints, mut outbound_endpoint) = {
            // Endpoint::new requires being inside the runtime context, else it
            // panics on its first internal `tokio::spawn`.
            let _guard = runtime.enter();
            let endpoints = server_sockets
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
            // The outbound direction runs on its own
            // client-only endpoint bound to `client_socket`, so inbound
            // traffic does not interfere with egress.
            let outbound_endpoint = Endpoint::new(
                EndpointConfig::default(),
                None, // No server_config on this endpoint
                client_socket,
                Arc::new(TokioRuntime),
            )
            .map_err(Error::Endpoint)?;
            (endpoints, outbound_endpoint)
        };
        outbound_endpoint.set_default_client_config(client_config);

        let client_stats: Arc<ClientStats> = Arc::default();
        let server_stats: Arc<ServerStats> = Arc::default();
        // Egress channel carries *distinct* messages.
        // Size it to 5 seconds of the per-peer send rate - votor rates are very low.
        let egress_cap = (max_datagrams_per_second_per_peer.ceil() as usize).saturating_mul(5);
        let (egress_sender, egress_receiver) = mpsc::channel(egress_cap);
        let shutdown = CancellationToken::new();
        let (identity_sender, identity_receiver) = watch::channel(None);
        let key_updater = Arc::new(KeyUpdater {
            sender: identity_sender,
        });

        // A receive-only endpoint with no peerlist has nothing to connect to
        // so we don't spawn OutboundLoop at all.
        if let Some(peerlist_receiver) = peerlist_receiver.clone() {
            let outbound = OutboundLoop::new(
                outbound_endpoint,
                local_pubkey,
                egress_receiver,
                identity_receiver.clone(),
                peerlist_receiver,
                shutdown.clone(),
                client_stats,
            );
            runtime.spawn(outbound.run());
        }

        // Shared inbound event channel: the accept loop forwards authenticated
        // connections, and per-connection read tasks report lifecycle events.
        let (events_sender, events_receiver) =
            mpsc::channel::<InboundEvent>(CONN_EVENT_CHANNEL_CAP);
        let accept = AcceptLoop::new(
            endpoints,
            identity_receiver,
            events_sender.clone(),
            server_stats.clone(),
            shutdown.clone(),
        );
        runtime.spawn(accept.run());
        let inbound = InboundLoop::new(
            ingress,
            ban_receiver,
            peerlist_receiver,
            events_sender,
            events_receiver,
            server_stats.clone(),
            shutdown.clone(),
            max_datagrams_per_second_per_peer,
        );
        runtime.spawn(inbound.run());

        Ok(Self {
            egress: egress_sender,
            key_updater,
            server_stats,
            shutdown,
        })
    }
}

impl Drop for QuicDatagramEndpoint {
    /// Cancel the spawned loops so a dropped handle can't leak them.
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}
