use {
    crate::{
        HEADER_LENGTH, IP_ECHO_SERVER_RESPONSE_LENGTH, MAX_PORT_COUNT_PER_MESSAGE,
        ip_echo_server::{IpEchoServerMessage, IpEchoServerResponse},
    },
    bytes::{BufMut, BytesMut},
    itertools::Itertools,
    log::*,
    std::{
        collections::{BTreeMap, HashMap, HashSet},
        io::ErrorKind,
        net::{IpAddr, SocketAddr, TcpListener, UdpSocket},
        sync::{Arc, RwLock},
        time::{Duration, Instant},
    },
    tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpSocket,
        task::JoinSet,
    },
};

#[derive(Debug, thiserror::Error)]
pub enum IpEchoClientError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Timeout(#[from] tokio::time::error::Elapsed),
    #[error(transparent)]
    Serialize(#[from] wincode::WriteError),
    #[error(transparent)]
    Deserialize(#[from] wincode::ReadError),
    #[error("{0}")]
    InvalidResponse(String),
}

/// Applies to all operations with the echo server.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(5);

/// Make a request to the echo server, binding the client socket to the provided IP.
pub(crate) async fn ip_echo_server_request_with_binding(
    ip_echo_server_addr: SocketAddr,
    msg: IpEchoServerMessage,
    bind_address: IpAddr,
) -> Result<IpEchoServerResponse, IpEchoClientError> {
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
) -> Result<IpEchoServerResponse, IpEchoClientError> {
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
) -> Result<BytesMut, IpEchoClientError> {
    let mut stream = socket.connect(ip_echo_server_addr).await?;
    // Start with HEADER_LENGTH null bytes to avoid looking like an HTTP GET/POST request
    let mut bytes = BytesMut::with_capacity(IP_ECHO_SERVER_RESPONSE_LENGTH);
    bytes.extend_from_slice(&[0u8; HEADER_LENGTH]);
    bytes.extend_from_slice(&wincode::serialize(&msg)?);

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
) -> Result<IpEchoServerResponse, IpEchoClientError> {
    // It's common for users to accidentally confuse the validator's gossip port and JSON
    // RPC port.  Attempt to detect when this occurs by looking for the standard HTTP
    // response header and provide the user with a helpful error message
    if response.len() < HEADER_LENGTH {
        return Err(IpEchoClientError::InvalidResponse(format!(
            "Response too short, received {} bytes",
            response.len()
        )));
    }

    let (response_header, body) =
        response
            .split_first_chunk::<HEADER_LENGTH>()
            .ok_or_else(|| {
                IpEchoClientError::InvalidResponse(format!(
                    "Not enough data in the response from {ip_echo_server_addr}!"
                ))
            })?;
    let payload = match response_header {
        [0, 0, 0, 0] => wincode::deserialize(body)?,
        [b'H', b'T', b'T', b'P'] => {
            let http_response = std::str::from_utf8(body);
            match http_response {
                Ok(r) => {
                    return Err(IpEchoClientError::InvalidResponse(format!(
                        "Invalid gossip entrypoint. {ip_echo_server_addr} looks to be an HTTP \
                         port replying with {r}"
                    )));
                }
                Err(_) => {
                    return Err(IpEchoClientError::InvalidResponse(format!(
                        "Invalid gossip entrypoint. {ip_echo_server_addr} looks to be an HTTP \
                         port."
                    )));
                }
            }
        }
        _ => {
            return Err(IpEchoClientError::InvalidResponse(format!(
                "Invalid gossip entrypoint. {ip_echo_server_addr} provided unexpected header \
                 bytes {response_header:?} "
            )));
        }
    };
    Ok(payload)
}

pub(crate) const DEFAULT_RETRY_COUNT: usize = 5;

/// How often listeners are polled for the echo server's connect-back.
const TCP_ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Checks if all of the provided TCP ports are reachable by the machine at
/// `ip_echo_server_addr`. Tests must complete within timeout provided.
/// Tests will run concurrently to avoid head-of-line blocking. Will return false on any error.
/// Listeners are switched to non-blocking mode.
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

    // Chunk the port range into slices small enough to fit into one packet
    for chunk in &listeners.into_iter().chunks(MAX_PORT_COUNT_PER_MESSAGE) {
        let listeners = chunk.collect_vec();
        let ports = listeners
            .iter()
            .map(|l| l.local_addr().expect("Sockets should be bound").port())
            .collect_vec();
        info!("Checking that tcp ports {ports:?} are reachable from {ip_echo_server_addr:?}");

        // make request to the echo server
        if let Err(err) = ip_echo_server_request_with_binding(
            ip_echo_server_addr,
            IpEchoServerMessage::new(&ports, &[]),
            bind_address,
        )
        .await
        {
            error!("ip_echo_server request failed: {err}");
            return false;
        }

        // spawn checker to wait for reply
        let start = Instant::now();
        for (port, tcp_listener) in ports.into_iter().zip(listeners) {
            if let Err(err) = tcp_listener.set_nonblocking(true) {
                error!("Unable to set tcp/{port} non-blocking: {err}");
                return false;
            }
            let thread_handle = tokio::task::spawn_blocking(move || {
                debug!("Waiting for incoming connection on tcp/{port}");
                loop {
                    match tcp_listener.accept() {
                        Ok(_) => return true,
                        Err(err) if err.kind() != ErrorKind::WouldBlock => {
                            error!("Accept failed on tcp/{port}: {err}");
                        }
                        Err(_) => {}
                    }
                    if start.elapsed() >= timeout {
                        return false;
                    }
                    std::thread::sleep(TCP_ACCEPT_POLL_INTERVAL);
                }
            });
            checkers.push((port, thread_handle));
        }
    }

    // now wait for results from all the tasks we have spawned.
    let mut ok = true;
    for (port, thread_handle) in checkers {
        if thread_handle.await.expect("Thread should exit cleanly") {
            info!("tcp/{port} is reachable");
        } else {
            error!("Received no response at tcp/{port}, check your port configuration");
            ok = false;
        }
    }

    ok
}

/// Checks if all of the provided UDP ports on all of the provided IPs are
/// reachable by the machine at `ip_echo_server_addr`.
/// This function will test a few ports at a time, retrying if necessary.
/// Each attempt waits up to `timeout` for UDP responses. Failed echo requests
/// are retried no sooner than `timeout` after the request started.
/// A given amount of retries will be made to accommodate packet loss.
/// This function may panic.
///
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
    let mut ip_to_ports: HashMap<IpAddr, BTreeMap<u16, Vec<&UdpSocket>>> = HashMap::new();
    for &socket in sockets.iter() {
        let local_addr = socket.local_addr().expect("Socket must be bound");
        ip_to_ports
            .entry(local_addr.ip())
            .or_default()
            .entry(local_addr.port())
            .or_default()
            .push(socket);
    }
    for (bind_ip, ports_to_socks_map) in ip_to_ports {
        let ports: Vec<u16> = ports_to_socks_map.keys().copied().collect();

        info!("Checking that udp ports {ports:?} are reachable from bind IP {bind_ip:?}");

        'outer: for chunk_to_check in ports.chunks(MAX_PORT_COUNT_PER_MESSAGE) {
            let ports_to_check = chunk_to_check.to_vec();

            for attempt in 1..=retry_count {
                if attempt > 1 {
                    error!("There are some udp ports with no response!! Retrying...");
                }
                // clone off the sockets that use ports within our chunk
                let sockets_to_check: Vec<UdpSocket> = ports_to_check
                    .iter()
                    .flat_map(|port| ports_to_socks_map.get(port).unwrap())
                    .map(|&s| s.try_clone().expect("Unable to clone UDP socket"))
                    .collect();

                let request_start = Instant::now();
                if let Err(err) = ip_echo_server_request_with_binding(
                    ip_echo_server_addr,
                    IpEchoServerMessage::new(&[], &ports_to_check),
                    bind_ip,
                )
                .await
                {
                    warn!("ip_echo_server request failed: {err}");
                    // Pace retries so the network can recover after XDP setup.
                    if attempt < retry_count {
                        tokio::time::sleep(timeout.saturating_sub(request_start.elapsed())).await;
                    }
                    continue;
                }

                let reachable_ports = Arc::new(RwLock::new(HashSet::new()));
                // Spawn threads for each socket to check
                let mut checkers = JoinSet::new();
                for socket in sockets_to_check {
                    let port = socket.local_addr().expect("Socket should be bound").port();
                    let reachable_ports = reachable_ports.clone();

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
                            debug!("Waited for incoming datagram on udp/{port}: {recv_result:?}");

                            if recv_result.is_ok() {
                                reachable_ports.write().unwrap().insert(port);
                                break;
                            }
                        }

                        socket.set_read_timeout(original_read_timeout).unwrap();
                    });
                }
                loop {
                    let next = checkers.join_next().await;
                    let Some(r) = next else {
                        break;
                    };
                    r.expect("Threads should exit cleanly");
                }
                // Might have lost a UDP packet, check that all ports were reached
                let reachable_ports = Arc::into_inner(reachable_ports)
                    .expect("Single owner expected")
                    .into_inner()
                    .expect("No threads should hold the lock");
                info!(
                    "checked udp ports: {ports_to_check:?}, reachable udp ports: \
                     {reachable_ports:?}"
                );
                if reachable_ports.len() == ports_to_check.len() {
                    continue 'outer; // starts checking next chunk of ports, if any
                }
            }

            error!("Maximum retry count reached. Some ports for IP {bind_ip} unreachable.");
            return false;
        }
    }
    true
}
