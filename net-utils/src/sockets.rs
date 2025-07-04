use {
    crate::PortRange,
    log::warn,
    socket2::{Domain, SockAddr, Socket, Type},
    std::{
        io,
        net::{IpAddr, SocketAddr, TcpListener, UdpSocket},
        sync::atomic::{AtomicU16, Ordering},
    },
};
#[cfg(feature = "dev-context-only-utils")]
use {std::net::Ipv4Addr, tokio::net::UdpSocket as TokioUdpSocket};
// base port for deconflicted allocations
const BASE_PORT: u16 = 5000;
// how much to allocate per individual process.
// we expect to have at most 64 concurrent tests in CI at any moment on a given host.
const SLICE_PER_PROCESS: u16 = (u16::MAX - BASE_PORT) / 64;
/// Retrieve a free 20-port slice for unit tests
///
/// When running under nextest, this will try to provide
/// a unique slice of port numbers (assuming no other nextest processes
/// are running on the same host) based on NEXTEST_TEST_GLOBAL_SLOT variable
/// The port ranges will be reused following nextest logic.
///
/// When running without nextest, this will only bump an atomic and eventually
/// panic when it runs out of port numbers to assign.
#[allow(clippy::arithmetic_side_effects)]
pub fn localhost_port_range_for_tests() -> (u16, u16) {
    static SLICE: AtomicU16 = AtomicU16::new(0);
    let offset = SLICE.fetch_add(20, Ordering::Relaxed);
    let start = offset
        + match std::env::var("NEXTEST_TEST_GLOBAL_SLOT") {
            Ok(slot) => {
                let slot: u16 = slot.parse().unwrap();
                assert!(
                    offset < SLICE_PER_PROCESS,
                    "Overrunning into the port range of another test! Consider using fewer ports per test."
                );
                BASE_PORT + slot * SLICE_PER_PROCESS
            }
            Err(_) => BASE_PORT,
        };
    assert!(start < u16::MAX - 20, "ran out of port numbers!");
    (start, start + 20)
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

#[allow(deprecated)]
impl From<crate::SocketConfig> for SocketConfiguration {
    fn from(value: crate::SocketConfig) -> Self {
        Self {
            reuseport: value.reuseport,
            recv_buffer_size: value.recv_buffer_size,
            send_buffer_size: value.send_buffer_size,
            non_blocking: false,
        }
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

pub fn bind_with_any_port_with_config(
    ip_addr: IpAddr,
    config: SocketConfiguration,
) -> io::Result<UdpSocket> {
    let sock = udp_socket_with_config(config)?;
    let addr = SocketAddr::new(ip_addr, 0);
    let bind = sock.bind(&SockAddr::from(addr));
    match bind {
        Ok(_) => Result::Ok(sock.into()),
        Err(err) => Err(io::Error::other(format!("No available UDP port: {err}"))),
    }
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
            "multi_bind_in_range_with_config() only supports 1 socket on this platform ({} requested)",
            num
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
    bind_to_async(IpAddr::V4(Ipv4Addr::LOCALHOST), 0).await
}

#[cfg(feature = "dev-context-only-utils")]
pub async fn bind_to_unspecified_async() -> io::Result<TokioUdpSocket> {
    bind_to_async(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0).await
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
    Err(io::Error::other(
        "couldn't find two ports with the correct offset in range".to_string(),
    ))
}

pub fn bind_more_with_config(
    socket: UdpSocket,
    num: usize,
    mut config: SocketConfiguration,
) -> io::Result<Vec<UdpSocket>> {
    if !PLATFORM_SUPPORTS_SOCKET_CONFIGS {
        if num > 1 {
            warn!(
                "bind_more_with_config() only supports 1 socket on this platform ({} requested)",
                num
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
#[allow(deprecated)]
mod tests {
    use {
        super::*,
        crate::{bind_in_range, sockets::localhost_port_range_for_tests},
        std::net::Ipv4Addr,
    };

    #[test]
    fn test_bind() {
        let (pr_s, pr_e) = localhost_port_range_for_tests();
        let ip_addr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
        let config = SocketConfiguration::default();
        let s = bind_in_range(ip_addr, (pr_s, pr_e)).unwrap();
        assert_eq!(s.0, pr_s, "bind_in_range should use first available port");
        let ip_addr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
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
        let ip_addr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
        let config = SocketConfiguration::default();
        let x = bind_with_any_port_with_config(ip_addr, config).unwrap();
        let y = bind_with_any_port_with_config(ip_addr, config).unwrap();
        assert_ne!(
            x.local_addr().unwrap().port(),
            y.local_addr().unwrap().port()
        );
    }

    #[test]
    fn test_bind_in_range_nil() {
        let ip_addr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
        bind_in_range(ip_addr, (2000, 2000)).unwrap_err();
        bind_in_range(ip_addr, (2000, 1999)).unwrap_err();
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
        let (pr_s, pr_e) = localhost_port_range_for_tests();
        let config = SocketConfiguration::default();
        let (port, _sockets) =
            bind_common_in_range_with_config(ip_addr, (pr_s, pr_e), config).unwrap();
        assert!((pr_s..pr_e).contains(&port));

        bind_common_in_range_with_config(ip_addr, (port, port + 1), config).unwrap_err();
    }

    #[test]
    fn test_bind_two_in_range_with_offset() {
        solana_logger::setup();
        let config = SocketConfiguration::default();
        let ip_addr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
        let offset = 6;
        if let Ok(((port1, _), (port2, _))) =
            bind_two_in_range_with_offset_and_config(ip_addr, (1024, 65535), offset, config, config)
        {
            assert!(port2 == port1 + offset);
        }
        let offset = 42;
        if let Ok(((port1, _), (port2, _))) =
            bind_two_in_range_with_offset_and_config(ip_addr, (1024, 65535), offset, config, config)
        {
            assert!(port2 == port1 + offset);
        }
        assert!(bind_two_in_range_with_offset_and_config(
            ip_addr,
            (1024, 1044),
            offset,
            config,
            config
        )
        .is_err());
    }
}
