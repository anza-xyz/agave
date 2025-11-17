#[cfg(feature = "dev-context-only-utils")]
use tokio::net::UdpSocket as TokioUdpSocket;
use {
    crate::PortRange,
    log::warn,
    socket2::{Domain, SockAddr, Socket, Type},
    std::{
        io,
        net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, UdpSocket},
        ops::Range,
        sync::atomic::{AtomicU16, Ordering},
    },
};
// base port for deconflicted allocations
pub(crate) const UNIQUE_ALLOC_BASE_PORT: u16 = 2000;
// how much to allocate per individual process.
// we expect to have at most 64 concurrent tests in CI at any moment on a given host.
const SLICE_PER_PROCESS: u16 = (u16::MAX - UNIQUE_ALLOC_BASE_PORT) / 64;
/// When running under nextest, this will try to provide
/// a unique slice of port numbers (assuming no other nextest processes
/// are running on the same host) based on NEXTEST_TEST_GLOBAL_SLOT variable
/// The port ranges will be reused following nextest logic.
///
/// When running without nextest, this will only bump an atomic and eventually
/// panic when it runs out of port numbers to assign.
#[allow(clippy::arithmetic_side_effects)]
pub fn unique_port_range_for_tests(size: u16) -> Range<u16> {
    static SLICE: AtomicU16 = AtomicU16::new(0);
    let offset = SLICE.fetch_add(size, Ordering::SeqCst);
    let start = offset
        + match std::env::var("NEXTEST_TEST_GLOBAL_SLOT") {
            Ok(slot) => {
                let slot: u16 = slot.parse().unwrap();
                assert!(
                    offset < SLICE_PER_PROCESS,
                    "Overrunning into the port range of another test! Consider using fewer ports \
                     per test."
                );
                UNIQUE_ALLOC_BASE_PORT + slot * SLICE_PER_PROCESS
            }
            Err(_) => UNIQUE_ALLOC_BASE_PORT,
        };
    assert!(start < u16::MAX - size, "Ran out of port numbers!");
    start..start + size
}

/// Retrieve a free 25-port slice for unit tests
///
/// When running under nextest, this will try to provide
/// a unique slice of port numbers (assuming no other nextest processes
/// are running on the same host) based on NEXTEST_TEST_GLOBAL_SLOT variable
/// The port ranges will be reused following nextest logic.
///
/// When running without nextest, this will only bump an atomic and eventually
/// panic when it runs out of port numbers to assign.
pub fn localhost_port_range_for_tests() -> (u16, u16) {
    let pr = unique_port_range_for_tests(25);
    (pr.start, pr.end)
}

/// Bind a `UdpSocket` to a unique port.
pub fn bind_to_localhost_unique() -> io::Result<UdpSocket> {
    bind_to(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        unique_port_range_for_tests(1).start,
    )
}

pub fn bind_gossip_port_in_range(
    gossip_addr: &SocketAddr,
    port_range: PortRange,
    bind_ip_addr: IpAddr,
) -> (u16, (UdpSocket, TcpListener)) {
    let config = SocketConfiguration::default();
    if gossip_addr.port() != 0 {
        (
            gossip_addr.port(),
            bind_common_with_config(bind_ip_addr, gossip_addr.port(), config).unwrap_or_else(|e| {
                panic!("gossip_addr bind_to port {}: {}", gossip_addr.port(), e)
            }),
        )
    } else {
        bind_common_in_range_with_config(bind_ip_addr, port_range, config).expect("Failed to bind")
    }
}

/// True on platforms that support advanced socket configuration
pub(crate) const PLATFORM_SUPPORTS_SOCKET_CONFIGS: bool =
    cfg!(not(any(windows, target_os = "ios")));

#[derive(Clone, Copy, Debug, Default)]
pub struct SocketConfiguration {
    reuseport: bool, // controls SO_REUSEPORT, this is not intended to be set explicitly
    recv_buffer_size: Option<usize>,
    send_buffer_size: Option<usize>,
    non_blocking: bool,
}

impl SocketConfiguration {
    /// Sets the receive buffer size for the socket (no effect on windows/ios).
    ///
    /// **Note:** On Linux the kernel will double the value you specify.
    /// For example, if you specify `16MB`, the kernel will configure the
    /// socket to use `32MB`.
    /// See: https://man7.org/linux/man-pages/man7/socket.7.html: SO_RCVBUF
    pub fn recv_buffer_size(mut self, size: usize) -> Self {
        self.recv_buffer_size = Some(size);
        self
    }

    /// Sets the send buffer size for the socket (no effect on windows/ios)
    ///
    /// **Note:** On Linux the kernel will double the value you specify.
    /// For example, if you specify `16MB`, the kernel will configure the
    /// socket to use `32MB`.
    /// See: https://man7.org/linux/man-pages/man7/socket.7.html: SO_SNDBUF
    pub fn send_buffer_size(mut self, size: usize) -> Self {
        self.send_buffer_size = Some(size);
        self
    }

    /// Configure the socket for non-blocking IO
    pub fn set_non_blocking(mut self, non_blocking: bool) -> Self {
        self.non_blocking = non_blocking;
        self
    }
}

#[cfg(any(windows, target_os = "ios"))]
fn set_reuse_port<T>(_socket: &T) -> io::Result<()> {
    Ok(())
}

/// Sets SO_REUSEPORT on platforms that support it.
#[cfg(not(any(windows, target_os = "ios")))]
fn set_reuse_port<T>(socket: &T) -> io::Result<()>
where
    T: std::os::fd::AsFd,
{
    use nix::sys::socket::{setsockopt, sockopt::ReusePort};
    setsockopt(socket, ReusePort, &true).map_err(io::Error::from)
}

pub(crate) fn udp_socket_with_config(config: SocketConfiguration) -> io::Result<Socket> {
    let SocketConfiguration {
        reuseport,
        recv_buffer_size,
        send_buffer_size,
        non_blocking,
    } = config;
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, None)?;
    if PLATFORM_SUPPORTS_SOCKET_CONFIGS {
        // Set buffer sizes
        if let Some(recv_buffer_size) = recv_buffer_size {
            sock.set_recv_buffer_size(recv_buffer_size)?;
        }
        if let Some(send_buffer_size) = send_buffer_size {
            sock.set_send_buffer_size(send_buffer_size)?;
        }

        if reuseport {
            set_reuse_port(&sock)?;
        }
    }
    sock.set_nonblocking(non_blocking)?;
    Ok(sock)
}

/// Find a port in the given range with a socket config that is available for both TCP and UDP
pub fn bind_common_in_range_with_config(
    ip_addr: IpAddr,
    range: PortRange,
    config: SocketConfiguration,
) -> io::Result<(u16, (UdpSocket, TcpListener))> {
    for port in range.0..range.1 {
        if let Ok((sock, listener)) = bind_common_with_config(ip_addr, port, config) {
            return Result::Ok((sock.local_addr().unwrap().port(), (sock, listener)));
        }
    }

    Err(io::Error::other(format!(
        "No available TCP/UDP ports in {range:?}"
    )))
}

pub fn bind_in_range_with_config(
    ip_addr: IpAddr,
    range: PortRange,
    config: SocketConfiguration,
) -> io::Result<(u16, UdpSocket)> {
    let socket = udp_socket_with_config(config)?;

    for port in range.0..range.1 {
        let addr = SocketAddr::new(ip_addr, port);

        if socket.bind(&SockAddr::from(addr)).is_ok() {
            let udp_socket: UdpSocket = socket.into();
            return Result::Ok((udp_socket.local_addr().unwrap().port(), udp_socket));
        }
    }

    Err(io::Error::other(format!(
        "No available UDP ports in {range:?}"
    )))
}

/// binds num sockets to the same port in a range with config
pub fn multi_bind_in_range_with_config(
    ip_addr: IpAddr,
    range: PortRange,
    config: SocketConfiguration,
    mut num: usize,
) -> io::Result<(u16, Vec<UdpSocket>)> {
    if !PLATFORM_SUPPORTS_SOCKET_CONFIGS && num != 1 {
        // See https://github.com/solana-labs/solana/issues/4607
        warn!(
            "multi_bind_in_range_with_config() only supports 1 socket on this platform ({num} \
             requested)"
        );
        num = 1;
    }
    let (port, socket) = bind_in_range_with_config(ip_addr, range, config)?;
    let sockets = bind_more_with_config(socket, num, config)?;
    Ok((port, sockets))
}

pub fn bind_to(ip_addr: IpAddr, port: u16) -> io::Result<UdpSocket> {
    let config = SocketConfiguration {
        ..Default::default()
    };
    bind_to_with_config(ip_addr, port, config)
}

#[cfg(feature = "dev-context-only-utils")]
pub async fn bind_to_async(ip_addr: IpAddr, port: u16) -> io::Result<TokioUdpSocket> {
    let config = SocketConfiguration {
        non_blocking: true,
        ..Default::default()
    };
    let socket = bind_to_with_config(ip_addr, port, config)?;
    TokioUdpSocket::from_std(socket)
}

#[cfg(feature = "dev-context-only-utils")]
pub async fn bind_to_localhost_async() -> io::Result<TokioUdpSocket> {
    let port = unique_port_range_for_tests(1).start;
    bind_to_async(IpAddr::V4(Ipv4Addr::LOCALHOST), port).await
}

#[cfg(feature = "dev-context-only-utils")]
pub async fn bind_to_unspecified_async() -> io::Result<TokioUdpSocket> {
    let port = unique_port_range_for_tests(1).start;
    bind_to_async(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port).await
}

pub fn bind_to_with_config(
    ip_addr: IpAddr,
    port: u16,
    config: SocketConfiguration,
) -> io::Result<UdpSocket> {
    let sock = udp_socket_with_config(config)?;

    let addr = SocketAddr::new(ip_addr, port);

    sock.bind(&SockAddr::from(addr)).map(|_| sock.into())
}

/// binds both a UdpSocket and a TcpListener on the same port
pub fn bind_common_with_config(
    ip_addr: IpAddr,
    port: u16,
    config: SocketConfiguration,
) -> io::Result<(UdpSocket, TcpListener)> {
    let sock = udp_socket_with_config(config)?;

    let addr = SocketAddr::new(ip_addr, port);
    let sock_addr = SockAddr::from(addr);
    sock.bind(&sock_addr)
        .and_then(|_| TcpListener::bind(addr).map(|listener| (sock.into(), listener)))
}

pub fn bind_two_in_range_with_offset_and_config(
    ip_addr: IpAddr,
    range: PortRange,
    offset: u16,
    sock1_config: SocketConfiguration,
    sock2_config: SocketConfiguration,
) -> io::Result<((u16, UdpSocket), (u16, UdpSocket))> {
    if range.1.saturating_sub(range.0) < offset {
        return Err(io::Error::other(
            "range too small to find two ports with the correct offset".to_string(),
        ));
    }

    let max_start_port = range.1.saturating_sub(offset);
    for port in range.0..=max_start_port {
        let first_bind_result = bind_to_with_config(ip_addr, port, sock1_config);
        if let Ok(first_bind) = first_bind_result {
            let second_port = port.saturating_add(offset);
            let second_bind_result = bind_to_with_config(ip_addr, second_port, sock2_config);
            if let Ok(second_bind) = second_bind_result {
                return Ok((
                    (first_bind.local_addr().unwrap().port(), first_bind),
                    (second_bind.local_addr().unwrap().port(), second_bind),
                ));
            }
        }
    }
    Err(io::Error::other(format!(
        "couldn't find two unused ports with offset {offset} in range {range:?}"
    )))
}

pub fn bind_more_with_config(
    socket: UdpSocket,
    num: usize,
    mut config: SocketConfiguration,
) -> io::Result<Vec<UdpSocket>> {
    if !PLATFORM_SUPPORTS_SOCKET_CONFIGS {
        if num > 1 {
            warn!(
                "bind_more_with_config() only supports 1 socket on this platform ({num} requested)"
            );
        }
        Ok(vec![socket])
    } else {
        set_reuse_port(&socket)?;
        config.reuseport = true;
        let addr = socket.local_addr().unwrap();
        let ip = addr.ip();
        let port = addr.port();
        std::iter::once(Ok(socket))
            .chain((1..num).map(|_| bind_to_with_config(ip, port, config)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            bind_in_range, get_cluster_shred_version, get_public_ip_addr_with_binding,
            ip_echo_client, ip_echo_server, parse_host,
            sockets::{localhost_port_range_for_tests, unique_port_range_for_tests},
            verify_all_reachable_tcp, verify_all_reachable_udp, DEFAULT_IP_ECHO_SERVER_THREADS,
            MAX_PORT_VERIFY_THREADS,
        },
        itertools::Itertools,
        std::{net::Ipv4Addr, time::Duration},
        tokio::runtime::Runtime,
    };

    fn runtime() -> Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Can not create a runtime")
    }

    #[test]
    fn test_bind() {
        let (pr_s, pr_e) = localhost_port_range_for_tests();
        let ip_addr = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let config = SocketConfiguration::default();
        let s = bind_in_range(ip_addr, (pr_s, pr_e)).unwrap();
        assert_eq!(s.0, pr_s, "bind_in_range should use first available port");
        let ip_addr = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let x = bind_to_with_config(ip_addr, pr_s + 1, config).unwrap();
        let y = bind_more_with_config(x, 2, config).unwrap();
        assert_eq!(
            y[0].local_addr().unwrap().port(),
            y[1].local_addr().unwrap().port()
        );
        bind_to_with_config(ip_addr, pr_s, SocketConfiguration::default()).unwrap_err();
        bind_in_range(ip_addr, (pr_s, pr_s + 2)).unwrap_err();

        let (port, v) =
            multi_bind_in_range_with_config(ip_addr, (pr_s + 5, pr_e), config, 10).unwrap();
        for sock in &v {
            assert_eq!(port, sock.local_addr().unwrap().port());
        }
    }

    #[test]
    fn test_bind_with_any_port() {
        let x = bind_to_localhost_unique().unwrap();
        let y = bind_to_localhost_unique().unwrap();
        assert_ne!(
            x.local_addr().unwrap().port(),
            y.local_addr().unwrap().port()
        );
    }

    #[test]
    fn test_bind_in_range_nil() {
        let ip_addr = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let range = unique_port_range_for_tests(2);
        bind_in_range(ip_addr, (range.end, range.end)).unwrap_err();
        bind_in_range(ip_addr, (range.end, range.start)).unwrap_err();
    }

    #[test]
    fn test_bind_on_top() {
        let config = SocketConfiguration::default();
        let localhost = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let port_range = localhost_port_range_for_tests();
        let (_p, s) = bind_in_range_with_config(localhost, port_range, config).unwrap();
        let _socks = bind_more_with_config(s, 8, config).unwrap();

        let _socks2 = multi_bind_in_range_with_config(localhost, port_range, config, 8).unwrap();
    }

    #[test]
    fn test_bind_common_in_range() {
        let ip_addr = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let range = unique_port_range_for_tests(5);
        let config = SocketConfiguration::default();
        let (port, _sockets) =
            bind_common_in_range_with_config(ip_addr, (range.start, range.end), config).unwrap();
        assert!(range.contains(&port));
        bind_common_in_range_with_config(ip_addr, (port, port + 1), config).unwrap_err();
    }

    #[test]
    fn test_bind_two_in_range_with_offset() {
        agave_logger::setup();
        let config = SocketConfiguration::default();
        let ip_addr = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let offset = 6;
        let port_range = unique_port_range_for_tests(10);
        if let Ok(((port1, _), (port2, _))) = bind_two_in_range_with_offset_and_config(
            ip_addr,
            (port_range.start, port_range.end),
            offset,
            config,
            config,
        ) {
            assert!(port2 == port1 + offset);
        }
        let offset = 42;
        if let Ok(((port1, _), (port2, _))) = bind_two_in_range_with_offset_and_config(
            ip_addr,
            (port_range.start, port_range.end),
            offset,
            config,
            config,
        ) {
            assert!(port2 == port1 + offset);
        }
        assert!(bind_two_in_range_with_offset_and_config(
            ip_addr,
            (port_range.start, port_range.start + 5),
            offset,
            config,
            config
        )
        .is_err());
    }

    #[test]
    fn test_get_public_ip_addr_none() {
        agave_logger::setup();
        let ip_addr = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let (pr_s, pr_e) = localhost_port_range_for_tests();
        let config = SocketConfiguration::default();
        let (_server_port, (server_udp_socket, server_tcp_listener)) =
            bind_common_in_range_with_config(ip_addr, (pr_s, pr_e), config).unwrap();

        let _runtime = ip_echo_server(
            server_tcp_listener,
            DEFAULT_IP_ECHO_SERVER_THREADS,
            /*shred_version=*/ Some(42),
        );

        let server_ip_echo_addr = server_udp_socket.local_addr().unwrap();
        assert_eq!(
            get_public_ip_addr_with_binding(
                &server_ip_echo_addr,
                IpAddr::V4(Ipv4Addr::UNSPECIFIED)
            )
            .unwrap(),
            parse_host("127.0.0.1").unwrap(),
        );
        assert_eq!(get_cluster_shred_version(&server_ip_echo_addr).unwrap(), 42);
        assert!(verify_all_reachable_tcp(&server_ip_echo_addr, vec![],));
        assert!(verify_all_reachable_udp(&server_ip_echo_addr, &[],));
    }

    #[test]
    fn test_get_public_ip_addr_reachable() {
        agave_logger::setup();
        let ip_addr = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let port_range = localhost_port_range_for_tests();
        let config = SocketConfiguration::default();
        let (_server_port, (server_udp_socket, server_tcp_listener)) =
            bind_common_in_range_with_config(ip_addr, port_range, config).unwrap();
        let (_client_port, (client_udp_socket, client_tcp_listener)) =
            bind_common_in_range_with_config(ip_addr, port_range, config).unwrap();

        let _runtime = ip_echo_server(
            server_tcp_listener,
            DEFAULT_IP_ECHO_SERVER_THREADS,
            /*shred_version=*/ Some(65535),
        );

        let ip_echo_server_addr = server_udp_socket.local_addr().unwrap();
        assert_eq!(
            get_public_ip_addr_with_binding(
                &ip_echo_server_addr,
                IpAddr::V4(Ipv4Addr::UNSPECIFIED)
            )
            .unwrap(),
            parse_host("127.0.0.1").unwrap(),
        );
        assert_eq!(
            get_cluster_shred_version(&ip_echo_server_addr).unwrap(),
            65535
        );
        assert!(verify_all_reachable_tcp(
            &ip_echo_server_addr,
            vec![client_tcp_listener],
        ));
        assert!(verify_all_reachable_udp(
            &ip_echo_server_addr,
            &[&client_udp_socket],
        ));
    }

    #[test]
    fn test_verify_ports_tcp_unreachable() {
        agave_logger::setup();
        let ip_addr = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let port_range = localhost_port_range_for_tests();
        let config = SocketConfiguration::default();
        let (_server_port, (server_udp_socket, _server_tcp_listener)) =
            bind_common_in_range_with_config(ip_addr, port_range, config).unwrap();

        // make the socket unreachable by not running the ip echo server!
        let server_ip_echo_addr = server_udp_socket.local_addr().unwrap();

        let (_, (_client_udp_socket, client_tcp_listener)) =
            bind_common_in_range_with_config(ip_addr, port_range, config).unwrap();

        let rt = runtime();
        assert!(!rt.block_on(ip_echo_client::verify_all_reachable_tcp(
            server_ip_echo_addr,
            vec![client_tcp_listener],
            Duration::from_secs(2),
        )));
    }

    #[test]
    fn test_verify_ports_udp_unreachable() {
        agave_logger::setup();
        let ip_addr = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let port_range = unique_port_range_for_tests(2);
        let config = SocketConfiguration::default();
        let (_server_port, (server_udp_socket, _server_tcp_listener)) =
            bind_common_in_range_with_config(ip_addr, (port_range.start, port_range.end), config)
                .unwrap();

        // make the socket unreachable by not running the ip echo server!
        let server_ip_echo_addr = server_udp_socket.local_addr().unwrap();

        let (_correct_client_port, (client_udp_socket, _client_tcp_listener)) =
            bind_common_in_range_with_config(ip_addr, (port_range.start, port_range.end), config)
                .unwrap();

        let rt = runtime();
        assert!(!rt.block_on(ip_echo_client::verify_all_reachable_udp(
            server_ip_echo_addr,
            &[&client_udp_socket],
            Duration::from_secs(2),
            3,
        )));
    }

    #[test]
    fn test_verify_many_ports_reachable() {
        agave_logger::setup();
        let ip_addr = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let config = SocketConfiguration::default();
        let mut tcp_listeners = vec![];
        let mut udp_sockets = vec![];

        let port_range = unique_port_range_for_tests(1);
        let (_server_port, (_, server_tcp_listener)) =
            bind_common_in_range_with_config(ip_addr, (port_range.start, port_range.end), config)
                .unwrap();
        for _ in 0..MAX_PORT_VERIFY_THREADS * 2 {
            let port_range = unique_port_range_for_tests(1);
            let (_client_port, (client_udp_socket, client_tcp_listener)) =
                bind_common_in_range_with_config(
                    ip_addr,
                    (port_range.start, port_range.end),
                    config,
                )
                .unwrap();
            tcp_listeners.push(client_tcp_listener);
            udp_sockets.push(client_udp_socket);
        }

        let ip_echo_server_addr = server_tcp_listener.local_addr().unwrap();

        let _runtime = ip_echo_server(
            server_tcp_listener,
            DEFAULT_IP_ECHO_SERVER_THREADS,
            Some(65535),
        );

        assert_eq!(
            get_public_ip_addr_with_binding(
                &ip_echo_server_addr,
                IpAddr::V4(Ipv4Addr::UNSPECIFIED)
            )
            .unwrap(),
            parse_host("127.0.0.1").unwrap(),
        );

        let socket_refs = udp_sockets.iter().collect_vec();
        assert!(verify_all_reachable_tcp(
            &ip_echo_server_addr,
            tcp_listeners,
        ));
        assert!(verify_all_reachable_udp(&ip_echo_server_addr, &socket_refs));
    }

    // This test is gated for non-macOS platforms because it requires binding to 127.0.0.2,
    // which is not supported on macOS by default.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn test_verify_udp_multiple_ips_reachable() {
        agave_logger::setup();
        let config = SocketConfiguration::default();
        let ip_a = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let ip_b = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2));

        let port_range = localhost_port_range_for_tests();

        let (_srv_udp_port, (srv_udp_sock, srv_tcp_listener)) =
            bind_common_in_range_with_config(ip_a, port_range, config).unwrap();

        let ip_echo_server_addr = srv_udp_sock.local_addr().unwrap();
        let _runtime = ip_echo_server(
            srv_tcp_listener,
            DEFAULT_IP_ECHO_SERVER_THREADS,
            /*shred_version=*/ Some(42),
        );

        let mut udp_sockets = Vec::new();
        let (_p1, (sock_a, _tl_a)) =
            bind_common_in_range_with_config(ip_a, port_range, config).unwrap();
        let (_p2, (sock_b, _tl_b)) =
            bind_common_in_range_with_config(ip_b, port_range, config).unwrap();

        udp_sockets.push(sock_a);
        udp_sockets.push(sock_b);

        let socket_refs: Vec<&UdpSocket> = udp_sockets.iter().collect();

        assert!(
            verify_all_reachable_udp(&ip_echo_server_addr, &socket_refs),
            "all UDP ports on both 127.0.0.1 and 127.0.0.2 should be reachable"
        );
    }
}
