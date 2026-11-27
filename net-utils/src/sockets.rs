#[cfg(feature = "dev-context-only-utils")]
use tokio::net::UdpSocket as TokioUdpSocket;
use {
    crate::{PortRange, test_port_allocator::unique_port_range_for_tests_internal},
    log::{error, info, warn},
    nix::sys::socket::{
        GetSockOpt, SetSockOpt, setsockopt,
        sockopt::{RcvBuf, ReusePort, SndBuf},
    },
    socket2::{Domain, Socket, Type},
    std::{
        io,
        net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, UdpSocket},
        ops::Range,
    },
};
// base port for deconflicted allocations
pub(crate) const UNIQUE_ALLOC_BASE_PORT: u16 = 2000;

/// Allocates a unique range of `size` ports for use in tests.
pub fn unique_port_range_for_tests(size: u16) -> Range<u16> {
    unique_port_range_for_tests_internal(size)
}

/// Retrieve a free 26-port slice for unit tests
///
/// When running under nextest, this will try to provide
/// a unique slice of port numbers (assuming no other nextest processes
/// are running on the same host) based on NEXTEST_TEST_GLOBAL_SLOT variable
/// The port ranges will be reused following nextest logic.
///
/// When running without nextest, this will only bump an atomic and eventually
/// panic when it runs out of port numbers to assign.
pub fn localhost_port_range_for_tests() -> (u16, u16) {
    let pr = unique_port_range_for_tests(26);
    (pr.start, pr.end)
}

/// Bind a `UdpSocket` to a unique port.
pub fn bind_to_localhost_unique() -> io::Result<UdpSocket> {
    SocketConfiguration::default().bind_udp_to(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        unique_port_range_for_tests(1).start,
    )
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
    /// See: <https://man7.org/linux/man-pages/man7/socket.7.html>: SO_RCVBUF
    pub fn recv_buffer_size(mut self, size: Option<usize>) -> Self {
        self.recv_buffer_size = size;
        self
    }

    /// Sets the send buffer size for the socket (no effect on windows/ios)
    ///
    /// **Note:** On Linux the kernel will double the value you specify.
    /// For example, if you specify `16MB`, the kernel will configure the
    /// socket to use `32MB`.
    /// See: <https://man7.org/linux/man-pages/man7/socket.7.html>: SO_SNDBUF
    pub fn send_buffer_size(mut self, size: Option<usize>) -> Self {
        self.send_buffer_size = size;
        self
    }

    /// Configure the socket for non-blocking IO
    pub fn set_non_blocking(mut self, non_blocking: bool) -> Self {
        self.non_blocking = non_blocking;
        self
    }

    /// Create an unbound UDP socket and set the socket options
    fn build_udp_socket(&self) -> io::Result<Socket> {
        let sock = Socket::new(Domain::IPV4, Type::DGRAM, None)?;
        if PLATFORM_SUPPORTS_SOCKET_CONFIGS {
            // Set buffer sizes
            if let Some(recv_buffer_size) = self.recv_buffer_size {
                sock.set_recv_buffer_size(recv_buffer_size)?;
            }
            if let Some(send_buffer_size) = self.send_buffer_size {
                sock.set_send_buffer_size(send_buffer_size)?;
            }

            if self.reuseport {
                set_reuse_port(&sock)?;
            }
        }
        sock.set_nonblocking(self.non_blocking)?;
        Ok(sock)
    }

    /// Create a UDP socket bound to a given IP address and port
    pub fn bind_udp_to(&self, ip_addr: IpAddr, port: u16) -> io::Result<UdpSocket> {
        let sock = self.build_udp_socket()?;
        let addr = SocketAddr::new(ip_addr, port);
        sock.bind(&addr.into())?;
        Ok(sock.into())
    }

    /// Create a UDP socket and a TCP listening socket,
    /// both bound to the same IP address and port
    pub fn bind_udp_tcp_to(
        &self,
        ip_addr: IpAddr,
        port: u16,
    ) -> io::Result<(UdpSocket, TcpListener)> {
        let addr = SocketAddr::new(ip_addr, port);
        let udp = self.build_udp_socket()?;
        udp.bind(&addr.into())?;
        let tcp = TcpListener::bind(addr)?;
        tcp.set_nonblocking(self.non_blocking)?;
        Ok((udp.into(), tcp))
    }

    /// Create a UDP socket and attempt to bind it to the first available port in the given range.
    /// If no port is available, the socket is closed and an error is returned.
    pub fn bind_udp_range(&self, ip_addr: IpAddr, range: PortRange) -> io::Result<UdpSocket> {
        let sock = self.build_udp_socket()?;
        for port in range.0..range.1 {
            let addr = SocketAddr::new(ip_addr, port).into();
            if sock.bind(&addr).is_ok() {
                return Ok(sock.into());
            }
        }

        Err(io::Error::other(format!(
            "No available UDP ports in {range:?}"
        )))
    }

    /// Create a UDP socket and a TCP listening socket, both bound to the same port in the given range.
    /// If no port is availble for both UDP and TCP, the sockets are closed and an error is returned.
    pub fn bind_udp_tcp_range(
        &self,
        ip_addr: IpAddr,
        range: PortRange,
    ) -> io::Result<(UdpSocket, TcpListener)> {
        for port in range.0..range.1 {
            if let Ok(pair) = self.bind_udp_tcp_to(ip_addr, port) {
                return Ok(pair);
            }
        }

        Err(io::Error::other(format!(
            "No available TCP/UDP ports in {range:?}"
        )))
    }

    /// Create a UDP socket and a TCP listening socket, both bound to the same IP address and port.
    /// If non-zero, use the given port otherwise use the first available port in the given range.
    pub fn bind_udp_tcp_to_or_range(
        &self,
        bind_ip_addr: IpAddr,
        port: u16,
        port_range: PortRange,
    ) -> io::Result<(UdpSocket, TcpListener)> {
        if port != 0 {
            self.bind_udp_tcp_to(bind_ip_addr, port)
        } else {
            self.bind_udp_tcp_range(bind_ip_addr, port_range)
        }
    }

    /// Take a bound UDP socket, set the SO_REUSEPORT socket option on it, and create N-1 additional
    /// UDP sockets bound to the same port. Note other socket options are set from the current
    /// SocketConfiguration object and are not copied from the passed in socket.
    pub fn bind_udp_reuse(&self, socket: UdpSocket, mut num: usize) -> io::Result<Vec<UdpSocket>> {
        if !PLATFORM_SUPPORTS_SOCKET_CONFIGS && num > 1 {
            warn!("bind_udp_reuse() only supports 1 socket on this platform ({num} requested)");
            num = 1;
        }
        set_reuse_port(&socket)?;
        let config = SocketConfiguration {
            reuseport: true,
            ..*self
        };
        let addr = socket.local_addr().unwrap();
        let ip = addr.ip();
        let port = addr.port();
        std::iter::once(Ok(socket))
            .chain((1..num).map(|_| config.bind_udp_to(ip, port)))
            .collect()
    }

    /// Return a vector of N UDP sockets bound to the same port in the given range.
    /// Note all sockets have the SO_REUSEPORT socket option set (depending on platfrom support).
    pub fn multi_bind_udp_range(
        &self,
        ip_addr: IpAddr,
        range: PortRange,
        num: usize,
    ) -> io::Result<Vec<UdpSocket>> {
        let sock = self.bind_udp_range(ip_addr, range)?;
        self.bind_udp_reuse(sock, num)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct UdpSocketBufferLimits {
    pub recv_limit: Option<usize>,
    pub send_limit: Option<usize>,
}

impl UdpSocketBufferLimits {
    pub fn new() -> Self {
        let recv_limit = probe_max_udp_socket_buffer_size_capped(RcvBuf)
            .inspect(|sz| {
                info!("Maximum UDP socket receive buffer size is {sz}");
            })
            .inspect_err(|err| {
                error!("Maximum UDP socket receive buffer size could not be determined: {err}")
            })
            .ok();
        let send_limit = probe_max_udp_socket_buffer_size_capped(SndBuf)
            .inspect(|sz| {
                info!("Maximum UDP socket send buffer size is {sz}");
            })
            .inspect_err(|err| {
                error!("Maximum UDP socket send buffer size could not be determined: {err}")
            })
            .ok();
        Self {
            recv_limit,
            send_limit,
        }
    }

    pub fn recv_clamp(&self, desired_size: usize) -> Option<usize> {
        Some(
            self.recv_limit
                .map_or(desired_size, |sz| std::cmp::min(desired_size, sz)),
        )
    }

    pub fn send_clamp(&self, desired_size: usize) -> Option<usize> {
        Some(
            self.send_limit
                .map_or(desired_size, |sz| std::cmp::min(desired_size, sz)),
        )
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
    setsockopt(socket, ReusePort, &true).map_err(io::Error::from)
}

/// The probe_max_udp_socket_buffer_size() functions below will probe buffer sizes
/// up to this limit which should be both sufficiently large and within the
/// allowable range on all the platforms below. It has been conservatively
/// set to 64MiB which is sufficient for 50ms's worth of 10Gbit/s line rate data.
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))]
const MAX_BUF_SIZE: usize = 67108864;

/// probe_max_udp_socket_buffer_size_capped() calls the platform specific probe
/// function and caps the result to MAX_BUF_SIZE. This is needed as the probe
/// function can return values in excess of MAX_BUF_SIZE in certain circumstances.
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))]
fn probe_max_udp_socket_buffer_size_capped<T>(sock_opt: T) -> io::Result<usize>
where
    T: GetSockOpt<Val = usize> + SetSockOpt<Val = usize>,
{
    probe_max_udp_socket_buffer_size(sock_opt).map(|sz| std::cmp::min(MAX_BUF_SIZE, sz))
}
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "freebsd")))]
fn probe_max_udp_socket_buffer_size_capped<T>(_sock_opt: T) -> io::Result<usize>
where
    T: GetSockOpt<Val = usize> + SetSockOpt<Val = usize>,
{
    Err(std::io::ErrorKind::Unsupported.into())
}

/// Linux normally always succeeds, clamping the size to the system limit.
/// A quirk of Linux is that it doubles buffer size internally and returns
/// the doubled buffer size when queried hence the division by two below.
#[cfg(target_os = "linux")]
fn probe_max_udp_socket_buffer_size<T>(sock_opt: T) -> io::Result<usize>
where
    T: GetSockOpt<Val = usize> + SetSockOpt<Val = usize>,
{
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, None)?;
    let mut sz = sock_opt.get(&sock)? / 2;
    if sz < MAX_BUF_SIZE && sock_opt.set(&sock, &MAX_BUF_SIZE).is_ok() {
        sz = sock_opt.get(&sock)? / 2;
    }
    Ok(sz)
}

/// MacOS will clamp the size to the system limit but will succeed the
/// first time if the clamped size is larger than the current value.
/// Subsequent attempts to increase a buffer that is already at max will
/// fail. Hence we intentionally disregard any error when requesting the
/// maximum buffer size.
#[cfg(target_os = "macos")]
fn probe_max_udp_socket_buffer_size<T>(sock_opt: T) -> io::Result<usize>
where
    T: GetSockOpt<Val = usize> + SetSockOpt<Val = usize>,
{
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, None)?;
    let mut sz = sock_opt.get(&sock)?;
    if sz < MAX_BUF_SIZE && sock_opt.set(&sock, &MAX_BUF_SIZE).is_ok() {
        sz = sock_opt.get(&sock)?;
    }
    Ok(sz)
}

/// FreeBSD will not clamp the requested size but rather will return an
/// error if the size requested exceeds the system limit. Thus a binary
/// search algorithm is employed to determine the maximum available size.
#[cfg(target_os = "freebsd")]
fn probe_max_udp_socket_buffer_size<T>(sock_opt: T) -> io::Result<usize>
where
    T: GetSockOpt<Val = usize> + SetSockOpt<Val = usize>,
{
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, None)?;
    let mut sz = sock_opt.get(&sock)?;
    #[allow(clippy::arithmetic_side_effects)]
    if sz < MAX_BUF_SIZE {
        let (mut lo, mut hi) = (sz + 1, MAX_BUF_SIZE);
        while lo <= hi {
            let mid = (lo + hi) / 2;
            if sock_opt.set(&sock, &mid).is_ok() {
                lo = mid + 1;
            } else {
                hi = mid - 1;
            }
        }
        sz = hi;
    }
    Ok(sz)
}

pub fn bind_to(ip_addr: IpAddr, port: u16) -> io::Result<UdpSocket> {
    SocketConfiguration::default().bind_udp_to(ip_addr, port)
}

#[cfg(feature = "dev-context-only-utils")]
pub async fn bind_to_async(ip_addr: IpAddr, port: u16) -> io::Result<TokioUdpSocket> {
    let socket = SocketConfiguration::default()
        .set_non_blocking(true)
        .bind_udp_to(ip_addr, port)?;
    TokioUdpSocket::from_std(socket)
}

#[cfg(feature = "dev-context-only-utils")]
pub async fn bind_to_localhost_async() -> io::Result<TokioUdpSocket> {
    let port = unique_port_range_for_tests(1).start;
    bind_to_async(IpAddr::V4(Ipv4Addr::LOCALHOST), port).await
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            DEFAULT_IP_ECHO_SERVER_THREADS, MAX_PORT_VERIFY_THREADS, get_cluster_shred_version,
            get_public_ip_addr_with_binding, ip_echo_client, ip_echo_server, parse_host,
            sockets::{localhost_port_range_for_tests, unique_port_range_for_tests},
            verify_all_reachable_tcp, verify_all_reachable_udp,
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
        let s = config.bind_udp_range(ip_addr, (pr_s, pr_e)).unwrap();
        let p = s.local_addr().unwrap().port();
        assert_eq!(p, pr_s, "bind_udp_range should use first available port");
        let ip_addr = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let x = config.bind_udp_to(ip_addr, pr_s + 1).unwrap();
        let y = config.bind_udp_reuse(x, 2).unwrap();
        assert_eq!(
            y[0].local_addr().unwrap().port(),
            y[1].local_addr().unwrap().port()
        );
        config.bind_udp_to(ip_addr, pr_s).unwrap_err();
        config
            .bind_udp_range(ip_addr, (pr_s, pr_s + 2))
            .unwrap_err();

        let v = config
            .multi_bind_udp_range(ip_addr, (pr_s + 5, pr_e), 10)
            .unwrap();
        let port = v.first().unwrap().local_addr().unwrap().port();
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
        let config = SocketConfiguration::default();
        config
            .bind_udp_range(ip_addr, (range.end, range.end))
            .unwrap_err();
        config
            .bind_udp_range(ip_addr, (range.end, range.start))
            .unwrap_err();
    }

    #[test]
    fn test_bind_on_top() {
        let config = SocketConfiguration::default();
        let localhost = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let port_range = localhost_port_range_for_tests();
        let s = config.bind_udp_range(localhost, port_range).unwrap();
        let _socks = config.bind_udp_reuse(s, 8).unwrap();

        let _socks2 = config
            .multi_bind_udp_range(localhost, port_range, 8)
            .unwrap();
    }

    #[test]
    fn test_bind_common_in_range() {
        let ip_addr = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let range = unique_port_range_for_tests(5);
        let config = SocketConfiguration::default();
        let sockets = config
            .bind_udp_tcp_range(ip_addr, (range.start, range.end))
            .unwrap();
        let port = sockets.0.local_addr().unwrap().port();
        assert_eq!(port, sockets.1.local_addr().unwrap().port());
        assert!(range.contains(&port));
        config
            .bind_udp_tcp_range(ip_addr, (port, port + 1))
            .unwrap_err();
    }

    #[test]
    fn test_get_public_ip_addr_none() {
        agave_logger::setup();
        let ip_addr = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let (pr_s, pr_e) = localhost_port_range_for_tests();
        let config = SocketConfiguration::default();
        let (server_udp_socket, server_tcp_listener) =
            config.bind_udp_tcp_range(ip_addr, (pr_s, pr_e)).unwrap();

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
        let (server_udp_socket, server_tcp_listener) =
            config.bind_udp_tcp_range(ip_addr, port_range).unwrap();
        let (client_udp_socket, client_tcp_listener) =
            config.bind_udp_tcp_range(ip_addr, port_range).unwrap();

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
        let (server_udp_socket, _server_tcp_listener) =
            config.bind_udp_tcp_range(ip_addr, port_range).unwrap();

        // make the socket unreachable by not running the ip echo server!
        let server_ip_echo_addr = server_udp_socket.local_addr().unwrap();

        let (_client_udp_socket, client_tcp_listener) =
            config.bind_udp_tcp_range(ip_addr, port_range).unwrap();

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
        let (server_udp_socket, _server_tcp_listener) = config
            .bind_udp_tcp_range(ip_addr, (port_range.start, port_range.end))
            .unwrap();

        // make the socket unreachable by not running the ip echo server!
        let server_ip_echo_addr = server_udp_socket.local_addr().unwrap();

        let (client_udp_socket, _client_tcp_listener) = config
            .bind_udp_tcp_range(ip_addr, (port_range.start, port_range.end))
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
        let (_, server_tcp_listener) = config
            .bind_udp_tcp_range(ip_addr, (port_range.start, port_range.end))
            .unwrap();
        for _ in 0..MAX_PORT_VERIFY_THREADS * 2 {
            let port_range = unique_port_range_for_tests(1);
            let (client_udp_socket, client_tcp_listener) = config
                .bind_udp_tcp_range(ip_addr, (port_range.start, port_range.end))
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
}
