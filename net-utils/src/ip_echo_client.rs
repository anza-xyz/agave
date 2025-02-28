use {
    crate::{
        ip_echo_server::{IpEchoServerMessage, IpEchoServerResponse},
        HEADER_LENGTH, IP_ECHO_SERVER_RESPONSE_LENGTH, MAX_PORT_COUNT_PER_MESSAGE,
    },
    anyhow::bail,
    bytes::{BufMut, BytesMut},
    itertools::Itertools,
    log::*,
    std::{
        collections::{BTreeMap, HashSet},
        net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket},
        sync::{Arc, RwLock},
        time::{Duration, Instant},
    },
    tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpSocket,
        sync::oneshot,
        task::JoinSet,
    },
};

/// Applies to all operations with the echo server.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(5);

/// Make a request to the echo server, binding the client socket to the provided IP.
pub(crate) async fn ip_echo_server_request_with_binding(
    ip_echo_server_addr: SocketAddr,
    msg: IpEchoServerMessage,
    bind_address: IpAddr,
) -> anyhow::Result<IpEchoServerResponse> {
    let socket = TcpSocket::new_v4()?;
    socket.bind(SocketAddr::new(bind_address, 0))?;

    let response =
        tokio::time::timeout(TIMEOUT, make_request(socket, ip_echo_server_addr, msg)).await??;
    parse_response(response, ip_echo_server_addr)
}

/// Make a request to the echo server, client socket will be bound by the OS.
pub(crate) async fn ip_echo_server_request(
    ip_echo_server_addr: SocketAddr,
    msg: IpEchoServerMessage,
) -> anyhow::Result<IpEchoServerResponse> {
    let socket = TcpSocket::new_v4()?;
    let response =
        tokio::time::timeout(TIMEOUT, make_request(socket, ip_echo_server_addr, msg)).await??;
    parse_response(response, ip_echo_server_addr)
}

/// Makes the request to the specified server and returns reply as Bytes.
async fn make_request(
    socket: TcpSocket,
    ip_echo_server_addr: SocketAddr,
    msg: IpEchoServerMessage,
) -> anyhow::Result<BytesMut> {
    let mut stream = socket.connect(ip_echo_server_addr).await?;
    // Start with HEADER_LENGTH null bytes to avoid looking like an HTTP GET/POST request
    let mut bytes = BytesMut::with_capacity(IP_ECHO_SERVER_RESPONSE_LENGTH);
    bytes.extend_from_slice(&[0u8; HEADER_LENGTH]);
    bytes.extend_from_slice(&bincode::serialize(&msg)?);

    // End with '\n' to make this request look HTTP-ish and tickle an error response back
    // from an HTTP server
    bytes.put_u8(b'\n');
    stream.write_all(&bytes).await?;
    stream.flush().await?;

    bytes.clear();
    let _n = stream.read_buf(&mut bytes).await?;
    stream.shutdown().await?;

    Ok(bytes)
}

fn parse_response(
    response: BytesMut,
    ip_echo_server_addr: SocketAddr,
) -> anyhow::Result<IpEchoServerResponse> {
    // It's common for users to accidentally confuse the validator's gossip port and JSON
    // RPC port.  Attempt to detect when this occurs by looking for the standard HTTP
    // response header and provide the user with a helpful error message
    if response.len() < HEADER_LENGTH {
        bail!("Response too short, received {} bytes", response.len());
    }

    let (response_header, body) =
        response
            .split_first_chunk::<HEADER_LENGTH>()
            .ok_or(anyhow::anyhow!(
                "Not enough data in the response from {ip_echo_server_addr}!"
            ))?;
    let payload = match response_header {
        [0, 0, 0, 0] => {
            bincode::deserialize(&response[HEADER_LENGTH..IP_ECHO_SERVER_RESPONSE_LENGTH])?
        }
        [b'H', b'T', b'T', b'P'] => {
            let http_response = std::str::from_utf8(body);
            match http_response {
                Ok(r) => bail!("Invalid gossip entrypoint. {ip_echo_server_addr} looks to be an HTTP port replying with {r}"),
                Err(_) => bail!("Invalid gossip entrypoint. {ip_echo_server_addr} looks to be an HTTP port."),
            }
        }
        _ => {
            bail!("Invalid gossip entrypoint. {ip_echo_server_addr} provided unexpected header bytes {response_header:?} ");
        }
    };
    Ok(payload)
}

pub(crate) const DEFAULT_RETRY_COUNT: usize = 5;

/// Checks if all of the provided TCP ports are reachable by the machine at
/// `ip_echo_server_addr`. Tests must complete within timeout provided.
/// Tests will run concurrently to avoid head-of-line blocking. Will return false on any error.
/// This function may panic.
/// All listening sockets should be bound to the same IP.
pub(crate) async fn verify_all_reachable_tcp(
    ip_echo_server_addr: SocketAddr,
    listeners: Vec<TcpListener>,
    timeout: Duration,
) -> bool {
    if listeners.is_empty() {
        warn!("No ports provided for verify_all_reachable_tcp to check");
        return true;
    }

    // Extract the bind address for requests to remote server
    let bind_address = listeners[0]
        .local_addr()
        .expect("Sockets should be bound")
        .ip();

    // Verify that all other sockets are bound to the same address
    for listener in listeners.iter() {
        let local_binding = listener.local_addr().expect("Sockets should be bound");
        assert_eq!(
            local_binding.ip(),
            bind_address,
            "All sockets should be bound to the same IP"
        );
    }
    let mut checkers = Vec::new();
    let mut ok = true;

    // Chunk the port range into slices small enough to fit into one packet
    for chunk in &listeners.into_iter().chunks(MAX_PORT_COUNT_PER_MESSAGE) {
        let listeners = chunk.collect_vec();
        let ports = listeners
            .iter()
            .map(|l| l.local_addr().expect("Sockets should be bound").port())
            .collect_vec();
        info!(
            "Checking that tcp ports {:?} are reachable from {:?}",
            &ports, ip_echo_server_addr
        );

        // make request to the echo server
        let _ = ip_echo_server_request_with_binding(
            ip_echo_server_addr,
            IpEchoServerMessage::new(&ports, &[]),
            bind_address,
        )
        .await
        .map_err(|err| warn!("ip_echo_server request failed: {}", err));

        // spawn checker to wait for reply
        // since we do not know if tcp_listeners are nonblocking, we have to run them in native threads.
        for (port, tcp_listener) in ports.into_iter().zip(listeners) {
            let listening_addr = tcp_listener.local_addr().unwrap();
            let (sender, receiver) = oneshot::channel();

            // Use blocking API since we have no idea if sockets given to us are nonblocking or not
            let thread_handle = tokio::task::spawn_blocking(move || {
                debug!("Waiting for incoming connection on tcp/{}", port);
                match tcp_listener.incoming().next() {
                    Some(_) => {
                        // ignore errors here since this can only happen if a timeout was detected.
                        // timeout drops the receiver part of the channel resulting in failure to send.
                        let _ = sender.send(());
                    }
                    None => warn!("tcp incoming failed"),
                }
            });

            // Set the timeout on the receiver
            let receiver = tokio::time::timeout(timeout, receiver);
            checkers.push((listening_addr, thread_handle, receiver));
        }
    }

    // now wait for notifications from all the tasks we have spawned.
    for (listening_addr, thread_handle, receiver) in checkers.drain(..) {
        match receiver.await {
            Ok(Ok(_)) => {
                info!("tcp/{} is reachable", listening_addr.port());
            }
            Ok(Err(_v)) => {
                unreachable!("The receive on oneshot channel should never fail");
            }
            Err(_t) => {
                error!(
                    "Received no response at tcp/{}, check your port configuration",
                    listening_addr.port()
                );
                // Ugh, std rustc doesn't provide accepting with timeout or restoring original
                // nonblocking-status of sockets because of lack of getter, only the setter...
                // So, to close the thread cleanly, just connect from here.
                // ref: https://github.com/rust-lang/rust/issues/31615
                TcpStream::connect_timeout(&listening_addr, timeout).unwrap();
                // Mark that we have found error, but do  not exit yet, as we will have stuck ports otherwise.
                ok = false;
            }
        }
        thread_handle.await.expect("Thread should exit cleanly")
    }

    ok
}

/// Checks if all of the provided UDP ports are reachable by the machine at
/// `ip_echo_server_addr`.
/// This function will test a few ports at a time, retrying if necessary.
/// Tests must complete within timeout provided, so a longer timeout may be
/// necessary if checking many ports.
/// A given amount of retries will be made to accommodate packet loss.
/// This function may panic.
/// This function assumes that all sockets are bound to the same IP.
pub(crate) async fn verify_all_reachable_udp(
    ip_echo_server_addr: SocketAddr,
    sockets: &[&UdpSocket],
    timeout: Duration,
    retry_count: usize,
) -> bool {
    if sockets.is_empty() {
        warn!("No ports provided for verify_all_reachable_udp to check");
        return true;
    }
    // Extract the bind_address for requests from the first socket, it should be same for all others too
    let bind_address = sockets[0]
        .local_addr()
        .expect("Sockets should be bound")
        .ip();
    // This function may get fed multiple sockets bound to the same port.
    // In such case we need to know which sockets are bound to each port,
    // as only one of them will receive a packet from echo server
    let mut ports_to_socks_map: BTreeMap<_, _> = BTreeMap::new();
    for &socket in sockets.iter() {
        let local_binding = socket.local_addr().expect("Sockets should be bound");
        assert_eq!(
            local_binding.ip(),
            bind_address,
            "All sockets should be bound to the same IP"
        );
        let port = local_binding.port();
        ports_to_socks_map
            .entry(port)
            .or_insert_with(Vec::new)
            .push(socket);
    }

    let ports: Vec<_> = ports_to_socks_map.into_iter().collect();

    info!(
        "Checking that udp ports {:?} are reachable from {:?}",
        ports.iter().map(|(port, _)| port).collect::<Vec<_>>(),
        ip_echo_server_addr
    );

    'outer: for chunk_to_check in ports.chunks(MAX_PORT_COUNT_PER_MESSAGE) {
        let ports_to_check = chunk_to_check
            .iter()
            .map(|(port, _)| *port)
            .collect::<Vec<_>>();

        for attempt in 0..retry_count {
            if attempt > 0 {
                error!("There are some udp ports with no response!! Retrying...");
            }
            // clone off the sockets that use ports within our chunk
            let sockets_to_check = chunk_to_check.iter().flat_map(|(_, sockets)| {
                sockets
                    .iter()
                    .map(|&s| s.try_clone().expect("Unable to clone udp socket"))
            });

            let _ = ip_echo_server_request_with_binding(
                ip_echo_server_addr,
                IpEchoServerMessage::new(&[], &ports_to_check),
                bind_address,
            )
            .await
            .map_err(|err| warn!("ip_echo_server request failed: {}", err));

            let reachable_ports = Arc::new(RwLock::new(HashSet::new()));
            // Spawn threads for each socket to check
            let mut checkers = JoinSet::new();
            for socket in sockets_to_check {
                let port = socket.local_addr().expect("Socket should be bound").port();
                let reachable_ports = reachable_ports.clone();

                // Use blocking API since we have no idea if sockets given to us are nonblocking or not
                checkers.spawn_blocking(move || {
                    let start = Instant::now();

                    let original_read_timeout = socket.read_timeout().unwrap();
                    socket
                        .set_read_timeout(Some(Duration::from_millis(250)))
                        .unwrap();

                    loop {
                        if reachable_ports.read().unwrap().contains(&port)
                            || Instant::now().duration_since(start) >= timeout
                        {
                            break;
                        }

                        let recv_result = socket.recv(&mut [0; 1]);
                        debug!(
                            "Waited for incoming datagram on udp/{}: {:?}",
                            port, recv_result
                        );

                        if recv_result.is_ok() {
                            reachable_ports.write().unwrap().insert(port);
                            break;
                        }
                    }
                    socket.set_read_timeout(original_read_timeout).unwrap();
                });
            }

            while let Some(r) = checkers.join_next().await {
                r.expect("Threads should exit cleanly");
            }

            // Might have lost a UDP packet, check that all ports were reached
            let reachable_ports = Arc::into_inner(reachable_ports)
                .expect("Single owner expected")
                .into_inner()
                .expect("No threads should hold the lock");
            info!(
                "checked udp ports: {:?}, reachable udp ports: {:?}",
                ports_to_check, reachable_ports
            );
            if reachable_ports.len() == ports_to_check.len() {
                continue 'outer; // starts checking next chunk of ports, if any
            }
        }
        error!("Maximum retry count is reached....");
        return false;
    }
    true
}
