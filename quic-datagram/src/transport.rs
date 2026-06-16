//! Transport tuning constants for a votor-style workload.
use {
    quinn::{
        AckFrequencyConfig, ClientConfig, IdleTimeout, ServerConfig, TransportConfig, VarInt,
        congestion::{Controller, ControllerFactory},
        crypto::rustls::{QuicClientConfig, QuicServerConfig},
    },
    quinn_proto::RttEstimator,
    rustls::pki_types::{CertificateDer, PrivateKeyDer},
    solana_keypair::{Keypair, Signer},
    solana_pubkey::Pubkey,
    solana_tls_utils::{
        new_dummy_x509_certificate, tls_client_config_builder, tls_server_config_builder,
    },
    std::{
        any::Any,
        sync::Arc,
        time::{Duration, Instant},
    },
};

/// Identity material derived from a keypair: the ed25519 pubkey plus the
/// self-signed TLS cert/key that the endpoint presents to peers.
pub(crate) struct IdentitySnapshot {
    pub pubkey: Pubkey,
    pub cert: CertificateDer<'static>,
    pub key: PrivateKeyDer<'static>,
}

impl IdentitySnapshot {
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
const DATAGRAM_RECEIVE_BUFFER_BYTES: usize = 8 * 1024;
/// Send datagram buffer (per connection).
/// Provisions enough room for 1 full slot of votes.
const DATAGRAM_SEND_BUFFER_BYTES: usize = 8 * 1024;
/// Close connections after this much time without feedback from peer.
/// This should be more than reasonable max time between PING (or data)
/// frames arriving from the peer. We also want to keep this rather short
/// to make sure we reinitiate connections that are truly stuck.
pub(crate) const MAX_IDLE_TIMEOUT: Duration = Duration::from_secs(5);
/// QUIC keep-alive heartbeat. Must be << `MAX_IDLE_TIMEOUT` to avoid
/// connections dying when no vote traffic is available, and > MAX_ACK_DELAY
/// to make sure we actually get ACKs for every keepalive we send.
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(2);
/// `max_ack_delay` we request the peer to use via the QUIC ACK Frequency
/// extension (RFC 9799). Loosens the peer's ACK cadence to cut
/// reverse-direction packet rate. Set this ~ 1 slot to avoid ACKs during
/// normal votor operation.
const MAX_ACK_DELAY: Duration = Duration::from_millis(400);
/// MTU used for all datagrams. Path-MTU discovery is disabled, so the initial
/// and minimum MTU are the same; 1280 is the QUIC-spec floor.
const DATAGRAM_MTU: u16 = 1280;

/// Allow this many bytes to be in flight towards any other peer
const NOP_CONGESTION_WINDOW: u64 = 8 * 1024 * 1024;

/// Max number of in-flight connection requests we will look at
const MAX_INCOMING: usize = 400;

#[derive(Clone)]
struct NopCongestion;

impl Controller for NopCongestion {
    fn on_congestion_event(&mut self, _: Instant, _: Instant, _: bool, _: u64) {}
    fn on_ack(&mut self, _: Instant, _: Instant, _: u64, _: bool, _: &RttEstimator) {}
    fn on_mtu_update(&mut self, _: u16) {}
    fn window(&self) -> u64 {
        NOP_CONGESTION_WINDOW
    }
    fn initial_window(&self) -> u64 {
        NOP_CONGESTION_WINDOW
    }
    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }
    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

impl ControllerFactory for NopCongestion {
    fn build(self: Arc<Self>, _: Instant, _: u16) -> Box<dyn Controller> {
        Box::new(NopCongestion)
    }
}

pub(crate) fn new_transport_config() -> TransportConfig {
    let max_idle =
        IdleTimeout::try_from(MAX_IDLE_TIMEOUT).expect("MAX_IDLE_TIMEOUT fits IdleTimeout");
    let mut ack_freq = AckFrequencyConfig::default();
    ack_freq.max_ack_delay(Some(MAX_ACK_DELAY));
    // prevent acks from being elicited by packet count
    ack_freq.ack_eliciting_threshold(VarInt::from_u32(512));
    // disable reordering notifications
    ack_freq.reordering_threshold(VarInt::from_u32(0));
    let mut c = TransportConfig::default();
    c.datagram_receive_buffer_size(Some(DATAGRAM_RECEIVE_BUFFER_BYTES))
        .datagram_send_buffer_size(DATAGRAM_SEND_BUFFER_BYTES)
        .initial_mtu(DATAGRAM_MTU)
        .min_mtu(DATAGRAM_MTU)
        .mtu_discovery_config(None)
        .keep_alive_interval(Some(KEEP_ALIVE_INTERVAL))
        .max_idle_timeout(Some(max_idle))
        .ack_frequency_config(Some(ack_freq))
        .congestion_controller_factory(Arc::new(NopCongestion))
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
