use {
    agave_web_transport::ServerBuilder,
    bytes::Bytes,
    solana_packet::{Meta, PACKET_DATA_SIZE},
    solana_perf::packet::{BytesPacket, PacketBatch},
    std::net::SocketAddr,
    tokio::task::JoinHandle,
    tokio_util::sync::CancellationToken,
};

const MAX_TX_SIZE: usize = PACKET_DATA_SIZE;

pub fn spawn_webtransport_server(
    addr: SocketAddr,
    cert_chain: Vec<rustls::pki_types::CertificateDer<'static>>,
    key: rustls::pki_types::PrivateKeyDer<'static>,
    packet_sender: crossbeam_channel::Sender<PacketBatch>,
    cancel: CancellationToken,
) -> std::io::Result<JoinHandle<()>> {
    let server = ServerBuilder::new(addr)
        .build(cert_chain, key)
        .map_err(|e| std::io::Error::other(format!("WebTransport server error: {e}")))?;

    let handle = tokio::spawn(run_server(server, packet_sender, cancel));

    Ok(handle)
}

async fn run_server(
    mut server: agave_web_transport::Server,
    packet_sender: crossbeam_channel::Sender<PacketBatch>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                log::info!("WebTransport server shutting down");
                break;
            }
            request = server.accept() => {
                let Some(request) = request else {
                    log::warn!("WebTransport server accept returned None");
                    break;
                };

                let sender = packet_sender.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_session(request, sender).await {
                        log::debug!("WebTransport session error: {e}");
                    }
                });
            }
        }
    }
}

async fn handle_session(
    request: agave_web_transport::Request,
    packet_sender: crossbeam_channel::Sender<PacketBatch>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let remote_addr = request.remote_address();
    let session = request.ok().await?;

    log::debug!("WebTransport session established from {remote_addr}");

    loop {
        let mut stream = session.accept_uni().await?;
        let data = stream.read_to_end(MAX_TX_SIZE).await?;

        if data.is_empty() {
            continue;
        }

        let mut meta = Meta::default();
        meta.size = data.len();
        meta.set_socket_addr(&remote_addr);

        let packet = BytesPacket::new(Bytes::from(data), meta);
        let batch = PacketBatch::Single(packet);

        if packet_sender.send(batch).is_err() {
            log::warn!("WebTransport packet_sender disconnected");
            break;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::unbounded;
    use solana_keypair::Keypair;
    use solana_tls_utils::new_dummy_x509_certificate;
    use std::time::Duration;
    use tokio::time::timeout;

    fn generate_test_certs() -> (
        Vec<rustls::pki_types::CertificateDer<'static>>,
        rustls::pki_types::PrivateKeyDer<'static>,
    ) {
        let keypair = Keypair::new();
        let (cert, key) = new_dummy_x509_certificate(&keypair);
        (vec![cert], key)
    }

    async fn create_test_client(
        server_addr: SocketAddr,
    ) -> Result<web_transport_quinn::Session, Box<dyn std::error::Error + Send + Sync>> {
        let client = web_transport_quinn::ClientBuilder::new()
            .dangerous()
            .with_no_certificate_verification()?;

        let url = url::Url::parse(&format!("https://{}", server_addr))?;
        let session = client.connect(url).await?;
        Ok(session)
    }

    #[tokio::test]
    async fn test_webtransport_server_receives_packet() {
        rustls::crypto::ring::default_provider()
            .install_default()
            .ok();

        let (cert_chain, key) = generate_test_certs();
        let (packet_sender, packet_receiver) = unbounded();
        let cancel = CancellationToken::new();

        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let server = ServerBuilder::new(addr)
            .build(cert_chain, key)
            .expect("Failed to build server");

        let server_addr = server.local_addr().expect("Failed to get local addr");

        let cancel_clone = cancel.clone();
        let server_handle = tokio::spawn(run_server(server, packet_sender, cancel_clone));

        tokio::time::sleep(Duration::from_millis(100)).await;

        let session = create_test_client(server_addr)
            .await
            .expect("Failed to connect client");

        let test_data = b"hello webtransport";
        let mut send_stream = session.open_uni().await.expect("Failed to open uni stream");
        send_stream
            .write_all(test_data)
            .await
            .expect("Failed to write");
        send_stream.finish().expect("Failed to finish stream");

        let received = timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(batch) = packet_receiver.try_recv() {
                    return batch;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("Timeout waiting for packet");

        assert_eq!(received.len(), 1);
        let packet = received.iter().next().unwrap();
        assert_eq!(packet.meta().size, test_data.len());
        assert_eq!(&packet.data(..test_data.len()).unwrap(), test_data);

        cancel.cancel();
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_webtransport_server_multiple_streams() {
        rustls::crypto::ring::default_provider()
            .install_default()
            .ok();

        let (cert_chain, key) = generate_test_certs();
        let (packet_sender, packet_receiver) = unbounded();
        let cancel = CancellationToken::new();

        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let server = ServerBuilder::new(addr)
            .build(cert_chain, key)
            .expect("Failed to build server");

        let server_addr = server.local_addr().expect("Failed to get local addr");

        let cancel_clone = cancel.clone();
        let server_handle = tokio::spawn(run_server(server, packet_sender, cancel_clone));

        tokio::time::sleep(Duration::from_millis(100)).await;

        let session = create_test_client(server_addr)
            .await
            .expect("Failed to connect client");

        let num_streams = 5;
        for i in 0..num_streams {
            let data = format!("packet {}", i);
            let mut send_stream = session.open_uni().await.expect("Failed to open uni stream");
            send_stream
                .write_all(data.as_bytes())
                .await
                .expect("Failed to write");
            send_stream.finish().expect("Failed to finish stream");
        }

        let mut received_count = 0;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);

        while received_count < num_streams && tokio::time::Instant::now() < deadline {
            if let Ok(batch) = packet_receiver.try_recv() {
                received_count += batch.len();
            } else {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }

        assert_eq!(received_count, num_streams);

        cancel.cancel();
        let _ = server_handle.await;
    }
}
