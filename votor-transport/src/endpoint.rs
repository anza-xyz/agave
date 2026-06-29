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
        })
    }
}

impl Drop for QuicDatagramEndpoint {
    /// Cancel the spawned loops so a dropped handle can't leak them.
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}
