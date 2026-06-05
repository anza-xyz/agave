//! QUIC datagram endpoint
use {
    crate::{
        ALPENGLOW_ALPN, EGRESS_CHANNEL_CAP, HANDSHAKE_GLOBAL_BURST, HANDSHAKE_GLOBAL_RATE,
        MAX_PEERS,
        allowlist::Allowlist,
        client::{OutboundEvent, OutboundLoop},
        error::Error,
        server::{InboundEvent, InboundLoop},
        transport::{IdentitySnapshot, new_client_config, new_server_config},
    },
    bytes::Bytes,
    crossbeam_channel::Sender,
    quinn::{Endpoint, EndpointConfig, TokioRuntime},
    solana_keypair::{Keypair, Signer},
    solana_net_utils::{banlist::Banlist, token_bucket::TokenBucket},
    solana_pubkey::{Pubkey, PubkeyHasherBuilder},
    solana_tls_utils::{NotifyKeyUpdate, new_dummy_x509_certificate},
    std::{
        collections::HashMap,
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

pub(crate) const METRICS_INTERVAL: Duration = Duration::from_secs(2);

/// Capacity of the task -> control-loop connection-event channel.
const CONN_EVENT_CHANNEL_CAP: usize = MAX_PEERS as usize;

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
    shutdown: CancellationToken,
}

impl QuicDatagramEndpoint {
    /// Construct a datagram-only QUIC endpoint bound to `socket`. Spawns the
    /// unified control loop on `runtime`. Received datagrams flow into
    /// `ingress` via `try_send`; full ingress channel results in a drop
    /// (counted in `datagram_ingress_dropped_channel_full`).
    ///
    /// `allowlist` is consulted once per new connection in either direction.
    /// `banlist` is consulted on every send and at handshake.
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
        let server_stats = Arc::default();
        let (egress_tx, egress_rx) = mpsc::channel(EGRESS_CHANNEL_CAP);
        let (out_events_tx, out_events_rx) = mpsc::channel::<OutboundEvent>(CONN_EVENT_CHANNEL_CAP);
        let (in_events_tx, in_events_rx) = mpsc::channel::<InboundEvent>(CONN_EVENT_CHANNEL_CAP);
        let shutdown = CancellationToken::new();
        let (id_tx, identity_rx) = watch::channel(None);
        let key_updater = Arc::new(KeyUpdater { tx: id_tx });

        // TODO: change this to only ever admit gossip-advertised IPs
        // Limits total TLS verification effort spent by all threads
        let handshake_global_limiter = TokenBucket::new(
            HANDSHAKE_GLOBAL_BURST,
            HANDSHAKE_GLOBAL_BURST,
            HANDSHAKE_GLOBAL_RATE,
        );

        let outbound = OutboundLoop {
            endpoint: endpoint.clone(),
            local_pubkey,
            generation: 0,
            egress_rx,
            banlist: banlist.clone(),
            identity_rx: identity_rx.clone(),
            outgoing: HashMap::with_hasher(PubkeyHasherBuilder::default()),
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
            stats: server_stats,
            shutdown: shutdown.clone(),
        };
        runtime.spawn(inbound.run());

        Ok(Self {
            egress: egress_tx,
            key_updater,
            shutdown,
        })
    }

    /// Initiate endpoint shutdown.
    pub fn close(&self) {
        self.shutdown.cancel();
    }
}
