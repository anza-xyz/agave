//! QUIC datagram endpoint
use {
    crate::{
        ALPENGLOW_ALPN, CONN_EVENT_CHANNEL_CAP, MAX_ALPENGLOW_VOTE_ACCOUNTS,
        allowlist::Allowlist,
        client::OutboundLoop,
        error::Error,
        server::{AcceptLoop, InboundEvent, InboundLoop},
        stats::QuicDatagramStats,
        transport::{IdentitySnapshot, new_client_config, new_server_config},
    },
    bytes::Bytes,
    crossbeam_channel::Sender,
    quinn::{Endpoint, EndpointConfig, TokioRuntime},
    solana_keypair::{Keypair, Signer},
    solana_net_utils::banlist::Banlist,
    solana_pubkey::Pubkey,
    solana_tls_utils::{NotifyKeyUpdate, new_dummy_x509_certificate},
    std::{
        net::{SocketAddr, UdpSocket},
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
    /// Construct a datagram-only QUIC endpoint. `server_sockets` back the
    /// inbound (we-accept) direction and are expected to be SO_REUSEPORT
    /// siblings bound to the same address to load-balance inbound datagrams
    /// across them. The outbound (we-dial, send-only) direction runs on a
    /// dedicated `client_socket` bound to its own port — NOT a member of the
    /// SO_REUSEPORT accept group. A dialing socket sharing the group's port
    /// would have peers' handshake replies load-balanced by the kernel across
    /// the sibling sockets.
    ///
    /// Spawns the inbound and outbound loops on `runtime`; dropping the handle
    /// cancels them. Received datagrams flow into `ingress` via `try_send`;
    /// full ingress channel results in a drop
    /// (counted in `datagram_ingress_dropped_channel_full`).
    ///
    /// `allowlist` and `banlist` define admission policy.
    pub fn spawn(
        runtime: &Handle,
        keypair: &Keypair,
        server_sockets: Vec<UdpSocket>,
        client_socket: UdpSocket,
        ingress: Sender<Datagram>,
        allowlist: Arc<dyn Allowlist>,
        banlist: Arc<Banlist<Pubkey>>,
        max_datagrams_per_second_per_peer: f64,
    ) -> Result<Self, Error> {
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
            // The outbound (we-dial, send-only) direction runs on its own
            // client-only endpoint bound to `client_socket`.
            let outbound_endpoint = Endpoint::new(
                EndpointConfig::default(),
                None,
                client_socket,
                Arc::new(TokioRuntime),
            )
            .map_err(Error::Endpoint)?;
            (endpoints, outbound_endpoint)
        };
        outbound_endpoint.set_default_client_config(client_config);

        let client_stats = Arc::default();
        let server_stats: Arc<QuicDatagramStats> = Arc::default();
        // The egress buffer must absorb an entire standstill-refresh broadcast
        // (and a concurrent fresh-vote broadcast) without dropping: the voting
        // thread fans a full second's worth of messages out to every peer in a
        // synchronous burst, while the outbound loop drains them one datagram at
        // a time. Size it to one second of the admissible per-peer send rate
        // across the full peer set, with a floor of one full single-message
        // broadcast.
        let egress_cap = (max_datagrams_per_second_per_peer.ceil() as usize)
            .saturating_mul(MAX_ALPENGLOW_VOTE_ACCOUNTS)
            .max(MAX_ALPENGLOW_VOTE_ACCOUNTS);
        let (egress_tx, egress_rx) = mpsc::channel(egress_cap);
        let shutdown = CancellationToken::new();
        let (id_tx, identity_rx) = watch::channel(None);
        let key_updater = Arc::new(KeyUpdater { tx: id_tx });

        let outbound = OutboundLoop::new(
            outbound_endpoint,
            local_pubkey,
            egress_rx,
            banlist.clone(),
            identity_rx.clone(),
            shutdown.clone(),
            client_stats,
        );
        runtime.spawn(outbound.run());

        // Shared inbound event channel: the accept loop forwards authenticated
        // connections, and per-connection read tasks report lifecycle events.
        let (events_tx, events_rx) = mpsc::channel::<InboundEvent>(CONN_EVENT_CHANNEL_CAP);
        let accept = AcceptLoop::new(
            endpoints,
            identity_rx,
            events_tx.clone(),
            server_stats.clone(),
            shutdown.clone(),
        );
        runtime.spawn(accept.run());
        let inbound = InboundLoop::new(
            ingress,
            banlist,
            allowlist,
            events_tx,
            events_rx,
            server_stats.clone(),
            shutdown.clone(),
            max_datagrams_per_second_per_peer,
        );
        runtime.spawn(inbound.run());

        Ok(Self {
            egress: egress_tx,
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
