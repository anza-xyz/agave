use {
    anyhow::Context as _,
    pem::Pem,
    quinn::{
        crypto::rustls::{QuicClientConfig, QuicServerConfig},
        ClientConfig, Connection, Endpoint, IdleTimeout, ServerConfig, TokioRuntime,
        TransportConfig,
    },
    solana_keypair::{Keypair, Signer},
    solana_pubkey::Pubkey,
    solana_tls_utils::{
        get_pubkey_from_tls_certificate, new_dummy_x509_certificate, tls_client_config_builder,
        tls_server_config_builder,
    },
    std::{net::SocketAddr, sync::Arc},
    tokio::time::{sleep, Duration},
};

pub const ALPN_QUIC_VOTOR: &[u8] = b"votor";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Shared UDP socket
    let addr: SocketAddr = "127.0.0.1:5000".parse()?;
    let udp = solana_net_utils::sockets::bind_to(addr.ip(), addr.port())?;
    udp.set_nonblocking(true)?;
    let client_keypair = Keypair::new();
    let server_keypair = Keypair::new();
    println!(
        "Server ID {} Client ID {}",
        server_keypair.pubkey(),
        client_keypair.pubkey()
    );
    let (server_config, server_cert_chain) = configure_server(&server_keypair)?;
    let client_config = configure_client(&client_keypair, &server_cert_chain)?;

    let runtime = Arc::new(TokioRuntime);
    // Create one endpoint with both roles
    let endpoint = Arc::new(Endpoint::new(
        quinn::EndpointConfig::default(),
        Some(server_config),
        udp,
        runtime,
    )?);

    // --- Server task ---
    tokio::spawn({
        let endpoint = endpoint.clone();
        async move {
            while let Some(incoming) = endpoint.accept().await {
                if incoming.remote_address_validated() {
                    incoming.retry().unwrap();
                    continue;
                }
                let connecting = incoming.accept().unwrap();
                let connection = connecting.await.unwrap();
                let remote_pubkey = get_remote_pubkey(&connection).unwrap();
                println!("Accepted connection from {remote_pubkey:?}");
                tokio::spawn(async move {
                    while let Ok(dg) = connection.read_datagram().await {
                        println!("Server received: {}", String::from_utf8_lossy(&dg));
                        let _ = connection.send_datagram(b"pong".to_vec().into());
                    }
                });
            }
        }
    });

    // --- Client part ---
    let client_connection = endpoint
        .connect_with(client_config, addr, "localhost")?
        .await?;

    //is this secure?
    let remote_pubkey = get_remote_pubkey(&client_connection).context("No server pubkey")?;
    println!("Established connection to {remote_pubkey:?}");
    client_connection.send_datagram(b"ping".to_vec().into())?;
    if let Ok(resp) = client_connection.read_datagram().await {
        println!("Client received: {}", String::from_utf8_lossy(&resp));
    }

    sleep(Duration::from_secs(1)).await;
    endpoint.close(0u32.into(), b"done");
    Ok(())
}

fn configure_client(keypair: &Keypair, _server_cert_pem: &str) -> anyhow::Result<ClientConfig> {
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

pub(crate) fn configure_server(keypair: &Keypair) -> anyhow::Result<(ServerConfig, String)> {
    let (cert, priv_key) = new_dummy_x509_certificate(keypair);
    let cert_chain_pem_parts = vec![Pem {
        tag: "CERTIFICATE".to_string(),
        contents: cert.as_ref().to_vec(),
    }];
    let cert_chain_pem = pem::encode_many(&cert_chain_pem_parts);

    let mut server_tls_config =
        tls_server_config_builder().with_single_cert(vec![cert], priv_key)?;
    server_tls_config.alpn_protocols = vec![ALPN_QUIC_VOTOR.to_vec()];
    let quic_server_config = QuicServerConfig::try_from(server_tls_config)?;

    let mut server_config = ServerConfig::with_crypto(Arc::new(quic_server_config));

    // disable path migration as we do not expect TPU clients to be on a mobile device
    server_config.migration(false);

    let transport_config = Arc::get_mut(&mut server_config.transport).unwrap();
    transport_config.max_concurrent_uni_streams(0_u8.into());
    transport_config.max_concurrent_bidi_streams(0_u8.into());
    transport_config.datagram_receive_buffer_size(Some(2048));

    let timeout = IdleTimeout::try_from(Duration::from_secs(2)).unwrap();
    transport_config.max_idle_timeout(Some(timeout));

    // Disable GSO.
    transport_config.enable_segmentation_offload(false);

    Ok((server_config, cert_chain_pem))
}

pub fn get_remote_pubkey(connection: &Connection) -> Option<Pubkey> {
    // Use the client cert only if it is self signed and the chain length is 1.
    connection
        .peer_identity()?
        .downcast::<Vec<rustls::pki_types::CertificateDer>>()
        .ok()
        .filter(|certs| certs.len() == 1)?
        .first()
        .and_then(get_pubkey_from_tls_certificate)
}
