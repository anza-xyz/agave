use {
    crate::TIMEOUT,
    log::*,
    serde_derive::{Deserialize, Serialize},
    solana_net_utils::bind_to_unspecified,
    solana_serde::default_on_eof,
    std::{
        io,
        net::{IpAddr, SocketAddr},
    },
    tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        time::timeout,
    },
};

/// Max number of ports in one message.
/// Changing this would be an ABI incompatible change.
pub(crate) const MAX_PORT_COUNT_PER_MESSAGE: usize = 4;
/// Length of the zeroes slice in the beginning of echo server responses
pub(crate) const SERVER_RESPONSE_PREFIX_LENGTH: usize = 4;
pub(crate) const SERVER_RESPONSE_TOTAL_LENGTH: usize = SERVER_RESPONSE_PREFIX_LENGTH + 23;

#[derive(Serialize, Deserialize, Default, Debug)]
pub(crate) struct IpEchoServerMessage {
    tcp_ports: [u16; MAX_PORT_COUNT_PER_MESSAGE], // Fixed size list of ports to avoid vec serde
    udp_ports: [u16; MAX_PORT_COUNT_PER_MESSAGE], // Fixed size list of ports to avoid vec serde
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpEchoServerResponse {
    // Public IP address of request echoed back to the node.
    pub(crate) address: IpAddr,
    // Cluster shred-version of the node running the server.
    #[serde(deserialize_with = "default_on_eof")]
    pub(crate) shred_version: Option<u16>,
}

impl IpEchoServerMessage {
    pub fn new(tcp_ports: &[u16], udp_ports: &[u16]) -> Self {
        let mut msg = Self::default();
        assert!(tcp_ports.len() <= msg.tcp_ports.len());
        assert!(udp_ports.len() <= msg.udp_ports.len());

        msg.tcp_ports[..tcp_ports.len()].copy_from_slice(tcp_ports);
        msg.udp_ports[..udp_ports.len()].copy_from_slice(udp_ports);
        msg
    }
}

pub(crate) fn ip_echo_server_request_length() -> usize {
    const REQUEST_TERMINUS_LENGTH: usize = 1;
    (SERVER_RESPONSE_PREFIX_LENGTH + REQUEST_TERMINUS_LENGTH)
        .wrapping_add(bincode::serialized_size(&IpEchoServerMessage::default()).unwrap() as usize)
}

async fn process_connection(
    mut socket: TcpStream,
    peer_addr: SocketAddr,
    shred_version: Option<u16>,
) -> io::Result<()> {
    info!("connection from {:?}", peer_addr);

    let mut data = vec![0u8; ip_echo_server_request_length()];

    let mut writer = {
        let (mut reader, writer) = socket.split();
        let _ = timeout(TIMEOUT, reader.read_exact(&mut data)).await??;
        writer
    };

    let request_header: String = data[0..SERVER_RESPONSE_PREFIX_LENGTH]
        .iter()
        .map(|b| *b as char)
        .collect();
    if request_header != "\0\0\0\0" {
        // Explicitly check for HTTP GET/POST requests to more gracefully handle
        // the case where a user accidentally tried to use a gossip entrypoint in
        // place of a JSON RPC URL:
        if request_header == "GET " || request_header == "POST" {
            // Send HTTP error response
            timeout(
                TIMEOUT,
                writer.write_all(b"HTTP/1.1 400 Bad Request\nContent-length: 0\n\n"),
            )
            .await??;
            return Ok(());
        }
        return Err(io::Error::other(format!(
            "Bad request header: {request_header}"
        )));
    }

    let msg = bincode::deserialize::<IpEchoServerMessage>(&data[SERVER_RESPONSE_PREFIX_LENGTH..])
        .map_err(|err| {
        io::Error::other(format!(
            "Failed to deserialize IpEchoServerMessage: {err:?}"
        ))
    })?;

    trace!("request: {:?}", msg);

    // Fire a datagram at each non-zero UDP port
    let udp_socket = bind_to_unspecified()?;
    for udp_port in &msg.udp_ports {
        if *udp_port != 0 {
            match udp_socket.send_to(&[0], SocketAddr::from((peer_addr.ip(), *udp_port))) {
                Ok(_) => debug!("Successful send_to udp/{}", udp_port),
                Err(err) => info!("Failed to send_to udp/{}: {}", udp_port, err),
            }
        }
    }

    // Try to connect to each non-zero TCP port
    for tcp_port in &msg.tcp_ports {
        if *tcp_port != 0 {
            debug!("Connecting to tcp/{}", tcp_port);

            let mut tcp_stream = timeout(
                TIMEOUT,
                TcpStream::connect(&SocketAddr::new(peer_addr.ip(), *tcp_port)),
            )
            .await??;

            debug!("Connection established to tcp/{}", *tcp_port);
            tcp_stream.shutdown().await?;
        }
    }

    let response = IpEchoServerResponse {
        address: peer_addr.ip(),
        shred_version,
    };
    // "\0\0\0\0" header is added to ensure a valid response will never
    // conflict with the first four bytes of a valid HTTP response.
    let mut bytes = vec![0u8; SERVER_RESPONSE_TOTAL_LENGTH];
    bincode::serialize_into(&mut bytes[SERVER_RESPONSE_PREFIX_LENGTH..], &response).unwrap();
    trace!("response: {:?}", bytes);
    writer.write_all(&bytes).await
}

/// Starts a simple TCP server that echos the IP address of any peer that connects
/// Used by functions like |get_public_ip_addr| and |get_cluster_shred_version|
pub async fn ip_echo_server(tcp_listener: std::net::TcpListener, shred_version: Option<u16>) {
    tcp_listener.set_nonblocking(true).unwrap();

    info!("bound to {:?}", tcp_listener.local_addr().unwrap());
    let tcp_listener =
        TcpListener::from_std(tcp_listener).expect("Failed to convert std::TcpListener");

    loop {
        match tcp_listener.accept().await {
            Ok((socket, peer_addr)) => {
                tokio::spawn(async move {
                    if let Err(err) = process_connection(socket, peer_addr, shred_version).await {
                        info!("session failed: {:?}", err);
                    }
                });
            }
            Err(err) => warn!("listener accept failed: {:?}", err),
        }
    }
}
