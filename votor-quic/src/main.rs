use {
    crate::{
        client::configure_client,
        common::get_remote_pubkey,
        server::{configure_server, run_server},
    },
    anyhow::Context as _,
    quinn::{Endpoint, TokioRuntime},
    solana_keypair::{Keypair, Signer},
    std::{collections::HashMap, net::SocketAddr, sync::Arc},
    tokio::{
        sync::Mutex,
        time::{sleep, Duration},
    },
};
mod client;
mod common;
mod server;

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
    let connection_table = Arc::new(Mutex::new(HashMap::new()));
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
    //let current_staked_nodes = ArcSwap::new(Arc::new(HashMap::new()));
    connection_table
        .lock()
        .await
        .insert(client_keypair.pubkey(), None);
    let (sender, _receiver) = crossbeam_channel::bounded(1024);
    // --- Server task ---
    tokio::spawn(run_server(endpoint.clone(), connection_table, sender));

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

    client_connection.close(0u8.into(), b"done");
    sleep(Duration::from_secs(1)).await;
    endpoint.close(0u32.into(), b"done");
    Ok(())
}
