use {
    crate::{common::get_remote_pubkey, common::ALPN_QUIC_VOTOR},
    bytes::Bytes,
    crossbeam_channel::Sender,
    pem::Pem,
    quinn::{crypto::rustls::QuicServerConfig, Connection, Endpoint, IdleTimeout, ServerConfig},
    solana_keypair::Keypair,
    solana_net_utils::token_bucket::TokenBucket,
    solana_pubkey::Pubkey,
    solana_tls_utils::{new_dummy_x509_certificate, tls_server_config_builder},
    std::{collections::HashMap, sync::Arc},
    tokio::{sync::Mutex, time::Duration},
};

pub async fn run_server(
    endpoint: Arc<Endpoint>,
    connection_table: Arc<Mutex<HashMap<Pubkey, Option<Connection>>>>,
    rx_datagrams: Sender<Bytes>,
) {
    while let Some(incoming) = endpoint.accept().await {
        let rate_limiter = TokenBucket::new(1000, 1000, 1000.0);
        if incoming.remote_address_validated() {
            incoming.retry().unwrap();
            continue;
        }
        if rate_limiter.consume_tokens(1).is_err() {
            incoming.refuse();
            continue;
        }
        let connecting = incoming.accept().unwrap();
        let connection = connecting.await.unwrap();
        let remote_pubkey = get_remote_pubkey(&connection).unwrap();
        {
            let guard = connection_table.lock().await;
            if !guard.contains_key(&remote_pubkey) {
                println!("Dropping connection from {remote_pubkey:?} - not allowed");
                connection.close(0u8.into(), b"not allowed");
            }
        }
        println!("Accepted connection from {remote_pubkey:?}");
        tokio::spawn({
            let rx_datagrams = rx_datagrams.clone();
            let connection_table = connection_table.clone();
            async move {
                let rate_limiter = TokenBucket::new(10, 10, 10.0);
                while let Ok(dg) = connection.read_datagram().await {
                    if rate_limiter.consume_tokens(1).is_err() {
                        connection.close(0u8.into(), b"not allowed");
                    }
                    println!("Server received: {}", String::from_utf8_lossy(&dg));
                    let _ = rx_datagrams.try_send(dg);
                    let _ = connection.send_datagram(b"pong".to_vec().into());
                }
                connection_table.lock().await.remove(&remote_pubkey);
                println!("Closing connection from {remote_pubkey:?}");
            }
        });
    }
}

pub fn configure_server(keypair: &Keypair) -> anyhow::Result<(ServerConfig, String)> {
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
