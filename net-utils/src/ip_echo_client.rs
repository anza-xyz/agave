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
    },
};

/// Applies to all operations with the echo server
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
        [0, 0, 0, 0] => bincode::deserialize(&response[HEADER_LENGTH..])?,
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
pub(crate) async fn verify_reachable_tcp_ports(
    ip_echo_server_addr: SocketAddr,
    listeners: Vec<(u16, TcpListener)>,
    timeout: Duration,
) -> bool {
    let mut checkers = Vec::new();
    let mut ok = true;

    // Chunk the port range into slices small enough to fit into one packet
    for chunk in &listeners.into_iter().chunks(MAX_PORT_COUNT_PER_MESSAGE) {
        let (ports, listeners): (Vec<_>, Vec<_>) = chunk.unzip();
        info!(
            "Checking that tcp ports {:?} are reachable from {:?}",
            &ports, ip_echo_server_addr
        );

        // make request to the echo server
        let _ = ip_echo_server_request(ip_echo_server_addr, IpEchoServerMessage::new(&ports, &[]))
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
                ok = false;
            }
        }
        thread_handle.await.expect("Thread should exit cleanly")
    }

    ok
}

/// Checks if all of the provided UDP ports are reachable by the machine at
/// `ip_echo_server_addr`. Tests must complete within timeout provided.
/// A given amount of retries will be made to accommodate packet loss.
pub(crate) async fn verify_reachable_udp_ports(
    ip_echo_server_addr: SocketAddr,
    udp_sockets: &[&UdpSocket],
    timeout: Duration,
    retry_count: usize,
) -> bool {
    let mut ok = true;
    let mut udp_ports: BTreeMap<_, _> = BTreeMap::new();
    udp_sockets.iter().for_each(|udp_socket| {
        let port = udp_socket.local_addr().unwrap().port();
        udp_ports
            .entry(port)
            .or_insert_with(Vec::new)
            .push(udp_socket);
    });
    let udp_ports: Vec<_> = udp_ports.into_iter().collect();

    info!(
        "Checking that udp ports {:?} are reachable from {:?}",
        udp_ports.iter().map(|(port, _)| port).collect::<Vec<_>>(),
        ip_echo_server_addr
    );

    'outer: for checked_ports_and_sockets in udp_ports.chunks(MAX_PORT_COUNT_PER_MESSAGE) {
        ok = false;

        for udp_remaining_retry in (0_usize..retry_count).rev() {
            let (checked_ports, checked_socket_iter) = (
                checked_ports_and_sockets
                    .iter()
                    .map(|(port, _)| *port)
                    .collect::<Vec<_>>(),
                checked_ports_and_sockets
                    .iter()
                    .flat_map(|(_, sockets)| sockets),
            );

            let _ = ip_echo_server_request(
                ip_echo_server_addr,
                IpEchoServerMessage::new(&[], &checked_ports),
            )
            .await
            .map_err(|err| warn!("ip_echo_server request failed: {}", err));

            // Spawn threads at once!
            let reachable_ports = Arc::new(RwLock::new(HashSet::new()));
            let thread_handles: Vec<_> = checked_socket_iter
                .map(|udp_socket| {
                    let port = udp_socket.local_addr().unwrap().port();
                    let udp_socket = udp_socket.try_clone().expect("Unable to clone udp socket");
                    let reachable_ports = reachable_ports.clone();

                    // Use blocking API since we have no idea if sockets given to us are nonblocking or not
                    tokio::task::spawn_blocking(move || {
                        let start = Instant::now();

                        let original_read_timeout = udp_socket.read_timeout().unwrap();
                        udp_socket
                            .set_read_timeout(Some(Duration::from_millis(250)))
                            .unwrap();
                        loop {
                            if reachable_ports.read().unwrap().contains(&port)
                                || Instant::now().duration_since(start) >= timeout
                            {
                                break;
                            }

                            let recv_result = udp_socket.recv(&mut [0; 1]);
                            debug!(
                                "Waited for incoming datagram on udp/{}: {:?}",
                                port, recv_result
                            );

                            if recv_result.is_ok() {
                                reachable_ports.write().unwrap().insert(port);
                                break;
                            }
                        }
                        udp_socket.set_read_timeout(original_read_timeout).unwrap();
                    })
                })
                .collect();

            // Now join threads!
            // Separate from the above by collect()-ing as an intermediately step to make the iterator
            // eager not lazy so that joining happens here at once after creating bunch of threads
            // at once.
            for thread in thread_handles {
                thread.await.expect("Threads should exit cleanly");
            }

            let reachable_ports = reachable_ports.read().unwrap().clone();
            if reachable_ports.len() == checked_ports.len() {
                info!(
                    "checked udp ports: {:?}, reachable udp ports: {:?}",
                    checked_ports, reachable_ports
                );
                ok = true;
                break;
            } else if udp_remaining_retry > 0 {
                // Might have lost a UDP packet, retry a couple times
                error!(
                    "checked udp ports: {:?}, reachable udp ports: {:?}",
                    checked_ports, reachable_ports
                );
                error!("There are some udp ports with no response!! Retrying...");
            } else {
                error!("Maximum retry count is reached....");
                break 'outer;
            }
        }
    }

    ok
}
