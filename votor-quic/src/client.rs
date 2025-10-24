use {
    crate::common::ALPN_QUIC_VOTOR,
    quinn::{crypto::rustls::QuicClientConfig, ClientConfig, IdleTimeout, TransportConfig},
    solana_keypair::Keypair,
    solana_tls_utils::{new_dummy_x509_certificate, tls_client_config_builder},
    std::sync::Arc,
    tokio::time::Duration,
};

pub fn configure_client(keypair: &Keypair, _server_cert_pem: &str) -> anyhow::Result<ClientConfig> {
    // TODO: verify server cert
    //let mut certs = rustls::RootCertStore::empty();
    //certs.add(CertificateDer::from_pem_slice(server_cert_pem.as_bytes())?)?;

    let (client_cert, client_key) = new_dummy_x509_certificate(keypair);

    let mut tls_client_config = tls_client_config_builder()
        .with_client_auth_cert(vec![client_cert], client_key)
        .expect("Failed to use client certificate");

    tls_client_config.enable_early_data = true;
    tls_client_config.alpn_protocols = vec![ALPN_QUIC_VOTOR.to_vec()];

    let mut quinn_client_config = ClientConfig::new(Arc::new(
        QuicClientConfig::try_from(tls_client_config).unwrap(),
    ));

    let mut transport_config = TransportConfig::default();
    let timeout = IdleTimeout::try_from(Duration::from_secs(2)).unwrap();
    transport_config.max_idle_timeout(Some(timeout));
    transport_config.max_concurrent_bidi_streams(0u8.into());
    transport_config.max_concurrent_uni_streams(0u8.into());
    // Disable GSO.
    transport_config.enable_segmentation_offload(false);
    quinn_client_config.transport_config(Arc::new(transport_config));

    Ok(quinn_client_config)
}
