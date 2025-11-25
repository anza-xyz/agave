#![cfg_attr(
    not(feature = "agave-unstable-api"),
    deprecated(
        since = "3.1.0",
        note = "This crate has been marked for formal inclusion in the Agave Unstable API. From \
                v4.0.0 onward, the `agave-unstable-api` crate feature must be specified to \
                acknowledge use of an interface that may break without warning."
    )
)]
//! The `net_utils` module assists with networking

// Activate some of the Rust 2024 lints to make the future migration easier.
#![warn(if_let_rescope)]
#![warn(keyword_idents_2024)]
#![warn(rust_2024_incompatible_pat)]
#![warn(tail_expr_drop_order)]
#![warn(unsafe_attr_outside_unsafe)]
#![warn(unsafe_op_in_unsafe_fn)]

mod ip_echo_client;
mod ip_echo_server;
pub mod multihomed_sockets;
pub mod socket_addr_space;
pub mod sockets;
pub mod token_bucket;

#[cfg(feature = "dev-context-only-utils")]
pub mod tooling_for_tests;

use {
    ip_echo_client::{ip_echo_server_request, ip_echo_server_request_with_binding},
    ip_echo_server::IpEchoServerMessage,
    rand::{rng, Rng},
    std::{
        io::{self},
        net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, ToSocketAddrs, UdpSocket},
    },
    url::Url,
};
pub use {
    ip_echo_server::{
        ip_echo_server, IpEchoServer, DEFAULT_IP_ECHO_SERVER_THREADS, MAX_PORT_COUNT_PER_MESSAGE,
        MINIMUM_IP_ECHO_SERVER_THREADS,
    },
    socket_addr_space::SocketAddrSpace,
};

/// A data type representing a public Udp socket
pub struct UdpSocketPair {
    pub addr: SocketAddr,    // Public address of the socket
    pub receiver: UdpSocket, // Locally bound socket that can receive from the public address
    pub sender: UdpSocket,   // Locally bound socket to send via public address
}

pub type PortRange = (u16, u16);

#[cfg(not(debug_assertions))]
/// Port range available to validator by default
pub const VALIDATOR_PORT_RANGE: PortRange = (8000, 10_000);

// Sets the port range outside of the region used by other tests to avoid interference
// This arrangement is not ideal, but can be removed once ConnectionCache is deprecated
#[cfg(debug_assertions)]
pub const VALIDATOR_PORT_RANGE: PortRange = (
    crate::sockets::UNIQUE_ALLOC_BASE_PORT - 512,
    crate::sockets::UNIQUE_ALLOC_BASE_PORT,
);

pub const MINIMUM_VALIDATOR_PORT_RANGE_WIDTH: u16 = 25; // VALIDATOR_PORT_RANGE must be at least this wide

pub(crate) const HEADER_LENGTH: usize = 4;
pub(crate) const IP_ECHO_SERVER_RESPONSE_LENGTH: usize = HEADER_LENGTH + 23;

/// Determine the public IP address of this machine by asking an ip_echo_server at the given
/// address. This function will bind to the provided bind_addreess.
pub fn get_public_ip_addr_with_binding(
    ip_echo_server_addr: &SocketAddr,
    bind_address: IpAddr,
) -> anyhow::Result<IpAddr> {
    let fut = ip_echo_server_request_with_binding(
        *ip_echo_server_addr,
        IpEchoServerMessage::default(),
        bind_address,
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let resp = rt.block_on(fut)?;
    Ok(resp.address)
}

/// Retrieves cluster shred version from Entrypoint address provided.
pub fn get_cluster_shred_version(ip_echo_server_addr: &SocketAddr) -> Result<u16, String> {
    let fut = ip_echo_server_request(*ip_echo_server_addr, IpEchoServerMessage::default());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let resp = rt.block_on(fut).map_err(|e| e.to_string())?;
    resp.shred_version
        .ok_or_else(|| "IP echo server does not return a shred-version".to_owned())
}

/// Retrieves cluster shred version from Entrypoint address provided,
/// binds client-side socket to the IP provided.
pub fn get_cluster_shred_version_with_binding(
    ip_echo_server_addr: &SocketAddr,
    bind_address: IpAddr,
) -> anyhow::Result<u16> {
    let fut = ip_echo_server_request_with_binding(
        *ip_echo_server_addr,
        IpEchoServerMessage::default(),
        bind_address,
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let resp = rt.block_on(fut)?;
    resp.shred_version
        .ok_or_else(|| anyhow::anyhow!("IP echo server does not return a shred-version"))
}

// Limit the maximum number of port verify threads to something reasonable
// in case the port ranges provided are very large.
const MAX_PORT_VERIFY_THREADS: usize = 64;

/// Checks if all of the provided UDP ports are reachable by the machine at
/// `ip_echo_server_addr`. Tests must complete within timeout provided.
/// Tests will run concurrently when possible, using up to 64 threads for IO.
/// This function assumes that all sockets are bound to the same IP, and will panic otherwise
pub fn verify_all_reachable_udp(
    ip_echo_server_addr: &SocketAddr,
    udp_sockets: &[&UdpSocket],
) -> bool {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(MAX_PORT_VERIFY_THREADS)
        .build()
        .expect("Tokio builder should be able to reliably create a current thread runtime");
    let fut = ip_echo_client::verify_all_reachable_udp(
        *ip_echo_server_addr,
        udp_sockets,
        ip_echo_client::TIMEOUT,
        ip_echo_client::DEFAULT_RETRY_COUNT,
    );
    rt.block_on(fut)
}

/// Checks if all of the provided TCP ports are reachable by the machine at
/// `ip_echo_server_addr`. Tests must complete within timeout provided.
/// Tests will run concurrently when possible, using up to 64 threads for IO.
/// This function assumes that all sockets are bound to the same IP, and will panic otherwise.
pub fn verify_all_reachable_tcp(
    ip_echo_server_addr: &SocketAddr,
    tcp_listeners: Vec<TcpListener>,
) -> bool {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(MAX_PORT_VERIFY_THREADS)
        .build()
        .expect("Tokio builder should be able to reliably create a current thread runtime");
    let fut = ip_echo_client::verify_all_reachable_tcp(
        *ip_echo_server_addr,
        tcp_listeners,
        ip_echo_client::TIMEOUT,
    );
    rt.block_on(fut)
}

pub fn parse_port_or_addr(optstr: Option<&str>, default_addr: SocketAddr) -> SocketAddr {
    if let Some(addrstr) = optstr {
        if let Ok(port) = addrstr.parse() {
            let mut addr = default_addr;
            addr.set_port(port);
            addr
        } else if let Ok(addr) = addrstr.parse() {
            addr
        } else {
            default_addr
        }
    } else {
        default_addr
    }
}

pub fn parse_port_range(port_range: &str) -> Option<PortRange> {
    let ports: Vec<&str> = port_range.split('-').collect();
    if ports.len() != 2 {
        return None;
    }

    let start_port = ports[0].parse();
    let end_port = ports[1].parse();

    if start_port.is_err() || end_port.is_err() {
        return None;
    }
    let start_port = start_port.unwrap();
    let end_port = end_port.unwrap();
    if end_port < start_port {
        return None;
    }
    Some((start_port, end_port))
}

pub fn parse_host(host: &str) -> Result<IpAddr, String> {
    // First, check if the host syntax is valid. This check is needed because addresses
    // such as `("localhost:1234", 0)` will resolve to IPs on some networks.
    let parsed_url = Url::parse(&format!("http://{host}")).map_err(|e| e.to_string())?;
    if parsed_url.port().is_some() {
        return Err(format!("Expected port in URL: {host}"));
    }

    // Next, check to see if it resolves to an IP address
    let ips: Vec<_> = (host, 0)
        .to_socket_addrs()
        .map_err(|err| err.to_string())?
        .map(|socket_address| socket_address.ip())
        .collect();
    if ips.is_empty() {
        Err(format!("Unable to resolve host: {host}"))
    } else {
        Ok(ips[0])
    }
}

pub fn is_host(string: String) -> Result<(), String> {
    parse_host(&string).map(|_| ())
}

pub fn parse_host_port(host_port: &str) -> Result<SocketAddr, String> {
    let addrs: Vec<_> = host_port
        .to_socket_addrs()
        .map_err(|err| format!("Unable to resolve host {host_port}: {err}"))?
        .collect();
    if addrs.is_empty() {
        Err(format!("Unable to resolve host: {host_port}"))
    } else {
        Ok(addrs[0])
    }
}

pub fn is_host_port(string: String) -> Result<(), String> {
    parse_host_port(&string).map(|_| ())
}

pub fn bind_in_range(ip_addr: IpAddr, range: PortRange) -> io::Result<(u16, UdpSocket)> {
    let config = sockets::SocketConfiguration::default();
    sockets::bind_in_range_with_config(ip_addr, range, config)
}

pub fn bind_to_localhost() -> io::Result<UdpSocket> {
    let config = sockets::SocketConfiguration::default();
    sockets::bind_to_with_config(IpAddr::V4(Ipv4Addr::LOCALHOST), 0, config)
}

pub fn bind_to_unspecified() -> io::Result<UdpSocket> {
    let config = sockets::SocketConfiguration::default();
    sockets::bind_to_with_config(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0, config)
}

/// Searches for an open port on a given binding ip_addr in the provided range.
///
/// This will start at a random point in the range provided, and search sequenctially.
/// If it can not find anything, an Error is returned.
///
/// Keep in mind this will not reserve the port for you, only find one that is empty.
pub fn find_available_port_in_range(ip_addr: IpAddr, range: PortRange) -> io::Result<u16> {
    let [port] = find_available_ports_in_range(ip_addr, range)?;
    Ok(port)
}

/// Searches for several ports on a given binding ip_addr in the provided range.
///
/// This will start at a random point in the range provided, and search sequentially.
/// If it can not find anything, an Error is returned.
pub fn find_available_ports_in_range<const N: usize>(
    ip_addr: IpAddr,
    range: PortRange,
) -> io::Result<[u16; N]> {
    let mut result = [0u16; N];
    let range = range.0..range.1;
    let mut next_port_to_try = range
        .clone()
        .cycle() // loop over the end of the range
        .skip(rng().random_range(range.clone()) as usize) // skip to random position
        .take(range.len()) // never take the same value twice
        .peekable();
    let mut num = 0;
    let config = sockets::SocketConfiguration::default();
    while num < N {
        let port_to_try = next_port_to_try.next().unwrap(); // this unwrap never fails since we exit earlier
        let bind = sockets::bind_common_with_config(ip_addr, port_to_try, config);
        match bind {
            Ok(_) => {
                result[num] = port_to_try;
                num = num.saturating_add(1);
            }
            Err(err) => {
                if next_port_to_try.peek().is_none() {
                    return Err(err);
                }
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use {
        super::*, ip_echo_server::IpEchoServerResponse, itertools::Itertools, std::net::Ipv4Addr,
    };

    #[test]
    fn test_response_length() {
        let resp = IpEchoServerResponse {
            address: IpAddr::from([u16::MAX; 8]), // IPv6 variant
            shred_version: Some(u16::MAX),
        };
        let resp_size = bincode::serialized_size(&resp).unwrap();
        assert_eq!(
            IP_ECHO_SERVER_RESPONSE_LENGTH,
            HEADER_LENGTH + resp_size as usize
        );
    }

    // Asserts that an old client can parse the response from a new server.
    #[test]
    fn test_backward_compat() {
        let address = IpAddr::from([
            525u16, 524u16, 523u16, 522u16, 521u16, 520u16, 519u16, 518u16,
        ]);
        let response = IpEchoServerResponse {
            address,
            shred_version: Some(42),
        };
        let mut data = vec![0u8; IP_ECHO_SERVER_RESPONSE_LENGTH];
        bincode::serialize_into(&mut data[HEADER_LENGTH..], &response).unwrap();
        data.truncate(HEADER_LENGTH + 20);
        assert_eq!(
            bincode::deserialize::<IpAddr>(&data[HEADER_LENGTH..]).unwrap(),
            address
        );
    }

    // Asserts that a new client can parse the response from an old server.
    #[test]
    fn test_forward_compat() {
        let address = IpAddr::from([
            525u16, 524u16, 523u16, 522u16, 521u16, 520u16, 519u16, 518u16,
        ]);
        let mut data = [0u8; IP_ECHO_SERVER_RESPONSE_LENGTH];
        bincode::serialize_into(&mut data[HEADER_LENGTH..], &address).unwrap();
        let response: Result<IpEchoServerResponse, _> =
            bincode::deserialize(&data[HEADER_LENGTH..]);
        assert_eq!(
            response.unwrap(),
            IpEchoServerResponse {
                address,
                shred_version: None,
            }
        );
    }

    #[test]
    fn test_parse_port_or_addr() {
        let p1 = parse_port_or_addr(Some("9000"), SocketAddr::from(([1, 2, 3, 4], 1)));
        assert_eq!(p1.port(), 9000);
        let p2 = parse_port_or_addr(Some("127.0.0.1:7000"), SocketAddr::from(([1, 2, 3, 4], 1)));
        assert_eq!(p2.port(), 7000);
        let p2 = parse_port_or_addr(Some("hi there"), SocketAddr::from(([1, 2, 3, 4], 1)));
        assert_eq!(p2.port(), 1);
        let p3 = parse_port_or_addr(None, SocketAddr::from(([1, 2, 3, 4], 1)));
        assert_eq!(p3.port(), 1);
    }

    #[test]
    fn test_parse_port_range() {
        assert_eq!(parse_port_range("garbage"), None);
        assert_eq!(parse_port_range("1-"), None);
        assert_eq!(parse_port_range("1-2"), Some((1, 2)));
        assert_eq!(parse_port_range("1-2-3"), None);
        assert_eq!(parse_port_range("2-1"), None);
    }

    #[test]
    fn test_parse_host() {
        parse_host("localhost:1234").unwrap_err();
        parse_host("localhost").unwrap();
        parse_host("127.0.0.0:1234").unwrap_err();
        parse_host("127.0.0.0").unwrap();
    }

    #[test]
    fn test_parse_host_port() {
        parse_host_port("localhost:1234").unwrap();
        parse_host_port("localhost").unwrap_err();
        parse_host_port("127.0.0.0:1234").unwrap();
        parse_host_port("127.0.0.0").unwrap_err();
    }

    #[test]
    fn test_is_host_port() {
        assert!(is_host_port("localhost:1234".to_string()).is_ok());
        assert!(is_host_port("localhost".to_string()).is_err());
    }

    #[test]
    fn test_find_available_port_in_range() {
        let ip_addr = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let range = sockets::unique_port_range_for_tests(4);
        let (pr_s, pr_e) = (range.start, range.end);
        assert_eq!(
            find_available_port_in_range(ip_addr, (pr_s, pr_s + 1)).unwrap(),
            pr_s
        );
        let port = find_available_port_in_range(ip_addr, (pr_s, pr_e)).unwrap();
        assert!((pr_s..pr_e).contains(&port));

        let _socket = sockets::bind_to(ip_addr, port).unwrap();
        find_available_port_in_range(ip_addr, (port, port + 1)).unwrap_err();
    }

    #[test]
    fn test_find_available_ports_in_range() {
        let ip_addr = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let port_range = sockets::localhost_port_range_for_tests();
        assert!(port_range.1 - port_range.0 > 16);
        // reserve 1 port to make it non-trivial
        let sock = sockets::bind_to_with_config(
            ip_addr,
            port_range.0 + 2,
            sockets::SocketConfiguration::default(),
        )
        .unwrap();
        let ports: [u16; 15] = find_available_ports_in_range(ip_addr, port_range).unwrap();
        let mut ports_vec = Vec::from(ports);
        ports_vec.push(sock.local_addr().unwrap().port());
        let res: Vec<_> = ports_vec.into_iter().unique().collect();
        assert_eq!(res.len(), 16, "Should reserve 16 unique ports");
    }
}
