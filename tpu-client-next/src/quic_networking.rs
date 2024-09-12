//! Utility code to handle quic networking.
use {
    quinn::{
        ClientConfig, ConnectError, Connection, ConnectionError, Endpoint, IdleTimeout,
        TransportConfig, WriteError,
    },
    solana_sdk::{
        quic::{QUIC_KEEP_ALIVE, QUIC_MAX_TIMEOUT},
        signature::Keypair,
    },
    solana_streamer::{
        nonblocking::quic::ALPN_TPU_PROTOCOL_ID, tls_certificates::new_dummy_x509_certificate,
    },
    std::{fmt, io, net::SocketAddr, sync::Arc},
    thiserror::Error,
};

/// Wrapper for [`std::io::Error`] implementing [`PartialEq`] to simplify error
/// checking for [`QuicError`] type. The reasons why [`std::io::Error`] doesn't
/// implement [`PartialEq`] are discussed in
/// <https://github.com/rust-lang/rust/issues/34158>
#[derive(Debug, Error)]
pub struct IoErrorWithPartialEq(pub io::Error);

impl PartialEq for IoErrorWithPartialEq {
    fn eq(&self, other: &Self) -> bool {
        let formatted_self = format!("{self:?}");
        let formatted_other = format!("{other:?}");
        formatted_self == formatted_other
    }
}

impl fmt::Display for IoErrorWithPartialEq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<io::Error> for IoErrorWithPartialEq {
    fn from(err: io::Error) -> Self {
        IoErrorWithPartialEq(err)
    }
}

/// Error types that can occur when dealing with QUIC connections or
/// transmissions.
#[derive(Error, Debug, PartialEq)]
pub enum QuicError {
    #[error(transparent)]
    WriteError(#[from] WriteError),
    #[error(transparent)]
    ConnectionError(#[from] ConnectionError),
    #[error(transparent)]
    ConnectError(#[from] ConnectError),
    #[error(transparent)]
    EndpointError(#[from] IoErrorWithPartialEq),
}

// Implementation of [`ServerCertVerifier`] that verifies everything as
// trustworthy.
struct SkipServerVerification;

impl SkipServerVerification {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}
impl rustls::client::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::Certificate,
        _intermediates: &[rustls::Certificate],
        _server_name: &rustls::ServerName,
        _scts: &mut dyn Iterator<Item = &[u8]>,
        _ocsp_response: &[u8],
        _now: std::time::SystemTime,
    ) -> Result<rustls::client::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::ServerCertVerified::assertion())
    }
}
pub(crate) struct QuicClientCertificate {
    pub certificate: rustls::Certificate,
    pub key: rustls::PrivateKey,
}

impl Default for QuicClientCertificate {
    fn default() -> Self {
        QuicClientCertificate::new(&Keypair::new())
    }
}

impl QuicClientCertificate {
    pub fn new(keypair: &Keypair) -> Self {
        let (certificate, key) = new_dummy_x509_certificate(keypair);
        Self { certificate, key }
    }
}

pub(crate) fn create_client_config(client_certificate: Arc<QuicClientCertificate>) -> ClientConfig {
    // adapted from QuicLazyInitializedEndpoint::create_endpoint
    let mut crypto = rustls::ClientConfig::builder()
        .with_safe_defaults()
        .with_custom_certificate_verifier(SkipServerVerification::new())
        .with_client_auth_cert(
            vec![client_certificate.certificate.clone()],
            client_certificate.key.clone(),
        )
        .expect("Failed to set QUIC client certificates");
    crypto.enable_early_data = true;
    crypto.alpn_protocols = vec![ALPN_TPU_PROTOCOL_ID.to_vec()];

    let mut config = ClientConfig::new(Arc::new(crypto));
    let mut transport_config = TransportConfig::default();

    let timeout = IdleTimeout::try_from(QUIC_MAX_TIMEOUT).unwrap();
    transport_config.max_idle_timeout(Some(timeout));
    transport_config.keep_alive_interval(Some(QUIC_KEEP_ALIVE));
    config.transport_config(Arc::new(transport_config));

    config
}

pub(crate) fn create_client_endpoint(
    bind_addr: SocketAddr,
    client_config: ClientConfig,
) -> Result<Endpoint, QuicError> {
    let mut endpoint = Endpoint::client(bind_addr).map_err(IoErrorWithPartialEq::from)?;
    endpoint.set_default_client_config(client_config);
    Ok(endpoint)
}

pub(crate) async fn send_data_over_stream(
    connection: &Connection,
    data: &[u8],
) -> Result<(), QuicError> {
    let mut send_stream = connection.open_uni().await?;
    send_stream.write_all(data).await?;
    // stream will be finished when dropped. Finishing here explicitly would
    // lead to blocking.
    Ok(())
}
