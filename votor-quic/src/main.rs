use quinn::{ClientConfig, Endpoint, ServerConfig, TokioRuntime};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use std::{net::SocketAddr, sync::Arc};
use tokio::time::{sleep, Duration};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Shared UDP socket
    let addr: SocketAddr = "127.0.0.1:5000".parse()?;
    let udp = solana_net_utils::sockets::bind_to(addr.ip(), addr.port())?;
    udp.set_nonblocking(true)?;
    //let udp = tokio::net::UdpSocket::from_std(udp)?;
    let (server_config, server_cert) = configure_server()?;
    let client_config = configure_client(&[&server_cert])?;
    // // Self-signed certificate
    // let cert_key = generate_simple_self_signed(vec!["localhost".into()])?;
    // let key_pem = cert_key.key_pair.serialize_pem();
    // let cert_der = CertificateDer::from_slice(cert_key.cert.der());

    // // Server config
    // let cert_chain = vec![cert_der];
    // let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes())?;
    // let mut server_cfg = ServerConfig::with_single_cert(cert_chain, key)?;
    // server_cfg.transport_config(Arc::new(quinn::TransportConfig::default()));

    // // Client config trusting that cert
    // let mut roots = rustls::RootCertStore::empty();
    // roots.add(cert_der)?;
    // let mut tls = rustls::ClientConfig::builder()
    //     .with_root_certificates(roots)
    //     .with_no_client_auth();
    // let client_cfg = ClientConfig::new(Arc::new(tls));

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
                let connecting = incoming.accept().unwrap();
                let connection = connecting.await.unwrap();
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

    client_connection.send_datagram(b"ping".to_vec().into())?;
    if let Ok(resp) = client_connection.read_datagram().await {
        println!("Client received: {}", String::from_utf8_lossy(&resp));
    }

    sleep(Duration::from_secs(1)).await;
    endpoint.close(0u32.into(), b"done");
    Ok(())
}

/// Constructs a QUIC endpoint configured for use a client only.
///
/// ## Args
///
/// - server_certs: list of trusted certificates.
#[allow(unused)]
pub fn make_client_endpoint(
    bind_addr: SocketAddr,
    server_certs: &[&[u8]],
) -> anyhow::Result<Endpoint> {
    let client_cfg = configure_client(server_certs)?;
    let mut endpoint = Endpoint::client(bind_addr)?;
    endpoint.set_default_client_config(client_cfg);
    Ok(endpoint)
}

/// Constructs a QUIC endpoint configured to listen for incoming connections on a certain address
/// and port.
///
/// ## Returns
///
/// - a stream of incoming QUIC connections
/// - server certificate serialized into DER format
#[allow(unused)]
pub fn make_server_endpoint(
    bind_addr: SocketAddr,
) -> anyhow::Result<(Endpoint, CertificateDer<'static>)> {
    let (server_config, server_cert) = configure_server()?;
    let endpoint = Endpoint::server(server_config, bind_addr)?;
    Ok((endpoint, server_cert))
}

/// Builds default quinn client config and trusts given certificates.
///
/// ## Args
///
/// - server_certs: a list of trusted certificates in DER format.
fn configure_client(server_certs: &[&[u8]]) -> anyhow::Result<ClientConfig> {
    let mut certs = rustls::RootCertStore::empty();
    for cert in server_certs {
        certs.add(CertificateDer::from(*cert))?;
    }

    Ok(ClientConfig::with_root_certificates(Arc::new(certs))?)
}

/// Returns default server configuration along with its certificate.
fn configure_server() -> anyhow::Result<(ServerConfig, CertificateDer<'static>)> {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert_der = CertificateDer::from(cert.cert);
    let priv_key = PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());

    let mut server_config =
        ServerConfig::with_single_cert(vec![cert_der.clone()], priv_key.into())?;
    let transport_config = Arc::get_mut(&mut server_config.transport).unwrap();
    transport_config.max_concurrent_uni_streams(0_u8.into());

    Ok((server_config, cert_der))
}

#[allow(unused)]
pub const ALPN_QUIC_HTTP: &[&[u8]] = &[b"hq-29"];
