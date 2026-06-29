//! Transport tuning constants for a votor-style workload.
use {
    crate::MAX_ALPENGLOW_VOTE_ACCOUNTS,
    quinn::{
        ClientConfig, IdleTimeout, ServerConfig, TransportConfig, VarInt,
        crypto::rustls::{QuicClientConfig, QuicServerConfig},
    },
    rustls::pki_types::{CertificateDer, PrivateKeyDer},
    solana_keypair::{Keypair, Signer},
    solana_pubkey::Pubkey,
    solana_tls_utils::{
        new_dummy_x509_certificate, tls_client_config_builder, tls_server_config_builder,
    },
    std::{sync::Arc, time::Duration},
};

/// Identity material derived from a keypair: the ed25519 pubkey plus the
/// self-signed TLS cert/key that the endpoint presents to peers.
pub(crate) struct Identity {
    pub pubkey: Pubkey,
    pub cert: CertificateDer<'static>,
    pub key: PrivateKeyDer<'static>,
}

impl Identity {
    pub fn from_keypair(keypair: &Keypair) -> Self {
        let (cert, key) = new_dummy_x509_certificate(keypair);
        Self {
            pubkey: keypair.pubkey(),
            cert,
            key,
        }
    }
}

/// Receive datagram buffer (per connection).
/// Provisions enough room for 1 full slot of votes.
const DATAGRAM_RECEIVE_BUFFER_BYTES: usize = 8 * DATAGRAM_MTU as usize;
/// Send datagram buffer (per connection).
/// Provisions enough room for a full standstill broadcast.
const DATAGRAM_SEND_BUFFER_BYTES: usize = 50 * DATAGRAM_MTU as usize;
/// Close connections after this much time without feedback from peer.
/// This should be more than reasonable max time between PING (or data)
/// frames arriving from the peer. We also want to keep this rather short
/// to make sure we reinitiate connections that are truly stuck.
pub(crate) const MAX_IDLE_TIMEOUT: Duration = Duration::from_secs(5);
/// QUIC keep-alive heartbeat. Must be << `MAX_IDLE_TIMEOUT` to avoid
/// connections dying when no vote traffic is available.
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(2);
/// MTU used for all datagrams. Path-MTU discovery is disabled, so the initial
/// and minimum MTU are the same; 1280 is the QUIC-spec floor.
const DATAGRAM_MTU: u16 = 1280;
/// Max number of buffered connection requests we will keep.
/// Sized such that a cluster-wide simultaneous reconnect fits.
const MAX_INCOMING: usize = MAX_ALPENGLOW_VOTE_ACCOUNTS;

pub(crate) fn new_transport_config() -> TransportConfig {
    let max_idle =
        IdleTimeout::try_from(MAX_IDLE_TIMEOUT).expect("MAX_IDLE_TIMEOUT fits IdleTimeout");
    let mut c = TransportConfig::default();
    c.datagram_receive_buffer_size(Some(DATAGRAM_RECEIVE_BUFFER_BYTES))
        .datagram_send_buffer_size(DATAGRAM_SEND_BUFFER_BYTES)
        .initial_mtu(DATAGRAM_MTU)
        .min_mtu(DATAGRAM_MTU)
        .mtu_discovery_config(None)
        .keep_alive_interval(Some(KEEP_ALIVE_INTERVAL))
        .max_idle_timeout(Some(max_idle))
        // Datagrams only - disable streams entirely.
        .max_concurrent_bidi_streams(VarInt::from(0u8))
        .max_concurrent_uni_streams(VarInt::from(0u8));
    c
}

/// Build the rustls + quinn server config.
#[allow(clippy::arithmetic_side_effects)]
pub(crate) fn new_server_config(
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
    alpn: &[u8],
) -> ServerConfig {
    let mut tls = tls_server_config_builder()
        .with_single_cert(vec![cert], key)
        .expect("rustls accepts our self-signed solana cert/key pair");
    tls.alpn_protocols = vec![alpn.to_vec()];
    tls.max_early_data_size = 0;
    let quic = QuicServerConfig::try_from(tls)
        .expect("TLS 1.3-only config yields an initial cipher suite");
    let mut cfg = ServerConfig::with_crypto(Arc::new(quic));
    cfg.incoming_buffer_size((DATAGRAM_MTU * 2) as u64);
    cfg.incoming_buffer_size_total(MAX_INCOMING as u64 * DATAGRAM_MTU as u64);
    cfg.max_incoming(MAX_INCOMING);
    cfg.retry_token_lifetime(MAX_IDLE_TIMEOUT);
    cfg.transport_config(Arc::new(new_transport_config()));
    cfg.migration(false);
    cfg
}

/// Build the rustls + quinn client config.
pub(crate) fn new_client_config(
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
    alpn: &[u8],
) -> ClientConfig {
    let mut tls = tls_client_config_builder()
        .with_client_auth_cert(vec![cert], key)
        .expect("rustls accepts our solana cert/key pair");
    tls.enable_early_data = false;
    tls.alpn_protocols = vec![alpn.to_vec()];
    let quic = QuicClientConfig::try_from(tls).expect("TLS config should be valid");
    let mut cfg = ClientConfig::new(Arc::new(quic));
    cfg.transport_config(Arc::new(new_transport_config()));
    cfg
}
