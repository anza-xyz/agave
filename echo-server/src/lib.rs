//! The `echo-server` crate contains the implementation of the IP echo server
//! and client, which are necessary for Solana entrypoint opeartion.
mod ip_echo_client;
mod ip_echo_server;

pub use ip_echo_server::ip_echo_server;
use {
    ip_echo_client::ip_echo_server_request_with_binding,
    ip_echo_server::IpEchoServerMessage,
    std::{
        net::{IpAddr, SocketAddr, TcpListener, UdpSocket},
        time::Duration,
    },
};
/// Default number of attempts when checking UDP port reachability
pub(crate) const UDP_PORT_CHECK_ATTEMPTS: usize = 5;
/// Applies to all operations with the echo server.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(5);
/// The maximum number of port verify threads. Chosen to be similar to number
/// ports solana might be opening
const MAX_PORT_VERIFY_THREADS: usize = 16;

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

/// Retrieves cluster shred version from Entrypoint address provided,
/// binds client-side socket to the IP provided.
pub fn get_cluster_shred_version(
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
        TIMEOUT,
        UDP_PORT_CHECK_ATTEMPTS,
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
    let fut =
        ip_echo_client::verify_all_reachable_tcp(*ip_echo_server_addr, tcp_listeners, TIMEOUT);
    rt.block_on(fut)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::ip_echo_server::{SERVER_RESPONSE_PREFIX_LENGTH, SERVER_RESPONSE_TOTAL_LENGTH},
        ip_echo_server::IpEchoServerResponse,
        itertools::Itertools,
        solana_net_utils::{
            bind_common_in_range_with_config, bind_in_range_with_config,
            sockets::localhost_port_range_for_tests, SocketConfig,
        },
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
    fn test_response_length() {
        let resp = IpEchoServerResponse {
            address: IpAddr::from([u16::MAX; 8]), // IPv6 variant
            shred_version: Some(u16::MAX),
        };
        let resp_size = bincode::serialized_size(&resp).unwrap();
        assert_eq!(
            SERVER_RESPONSE_TOTAL_LENGTH,
            SERVER_RESPONSE_PREFIX_LENGTH + resp_size as usize
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
        let mut data = vec![0u8; SERVER_RESPONSE_TOTAL_LENGTH];
        bincode::serialize_into(&mut data[SERVER_RESPONSE_PREFIX_LENGTH..], &response).unwrap();
        data.truncate(SERVER_RESPONSE_PREFIX_LENGTH + 20);
        assert_eq!(
            bincode::deserialize::<IpAddr>(&data[SERVER_RESPONSE_PREFIX_LENGTH..]).unwrap(),
            address
        );
    }

    // Asserts that a new client can parse the response from an old server.
    #[test]
    fn test_forward_compat() {
        let address = IpAddr::from([
            525u16, 524u16, 523u16, 522u16, 521u16, 520u16, 519u16, 518u16,
        ]);
        let mut data = [0u8; SERVER_RESPONSE_TOTAL_LENGTH];
        bincode::serialize_into(&mut data[SERVER_RESPONSE_PREFIX_LENGTH..], &address).unwrap();
        let response: Result<IpEchoServerResponse, _> =
            bincode::deserialize(&data[SERVER_RESPONSE_PREFIX_LENGTH..]);
        assert_eq!(
            response.unwrap(),
            IpEchoServerResponse {
                address,
                shred_version: None,
            }
        );
    }

    #[test]
    fn test_get_public_ip_addr_none() {
        solana_logger::setup();
        let ip_addr = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let (pr_s, pr_e) = localhost_port_range_for_tests();
        let config = SocketConfig::default();
        let (_server_port, (server_udp_socket, server_tcp_listener)) =
            bind_common_in_range_with_config(ip_addr, (pr_s, pr_e), config).unwrap();

        let rt = runtime();
        let _server = rt.spawn(ip_echo_server(
            server_tcp_listener,
            /*shred_version=*/ Some(42),
        ));

        let server_ip_echo_addr = server_udp_socket.local_addr().unwrap();
        assert_eq!(
            get_public_ip_addr_with_binding(&server_ip_echo_addr, ip_addr).unwrap(),
            ip_addr
        );
        assert_eq!(
            get_cluster_shred_version(&server_ip_echo_addr, ip_addr).unwrap(),
            42
        );
        assert!(verify_all_reachable_tcp(&server_ip_echo_addr, vec![],));
        assert!(verify_all_reachable_udp(&server_ip_echo_addr, &[],));
    }

    #[test]
    fn test_get_public_ip_addr_reachable() {
        solana_logger::setup();
        let ip_addr = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let port_range = localhost_port_range_for_tests();
        let config = SocketConfig::default();
        let (_server_port, (server_udp_socket, server_tcp_listener)) =
            bind_common_in_range_with_config(ip_addr, port_range, config).unwrap();
        let (_client_port, (client_udp_socket, client_tcp_listener)) =
            bind_common_in_range_with_config(ip_addr, port_range, config).unwrap();

        let rt = runtime();
        let _server = rt.spawn(ip_echo_server(
            server_tcp_listener,
            /*shred_version=*/ Some(65535),
        ));

        let ip_echo_server_addr = server_udp_socket.local_addr().unwrap();
        assert_eq!(
            get_public_ip_addr_with_binding(&ip_echo_server_addr, ip_addr).unwrap(),
            ip_addr,
        );
        assert_eq!(
            get_cluster_shred_version(&ip_echo_server_addr, ip_addr).unwrap(),
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

    #[tokio::test]
    async fn test_verify_ports_tcp_unreachable() {
        solana_logger::setup();
        let ip_addr = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let port_range = localhost_port_range_for_tests();
        let config = SocketConfig::default();
        let (_server_port, (server_udp_socket, _server_tcp_listener)) =
            bind_common_in_range_with_config(ip_addr, port_range, config).unwrap();

        // make the socket unreachable by not running the ip echo server!
        let server_ip_echo_addr = server_udp_socket.local_addr().unwrap();

        let (_, (_client_udp_socket, client_tcp_listener)) =
            bind_common_in_range_with_config(ip_addr, port_range, config).unwrap();

        assert!(
            !ip_echo_client::verify_all_reachable_tcp(
                server_ip_echo_addr,
                vec![client_tcp_listener],
                Duration::from_secs(2),
            )
            .await
        );
    }

    #[test]
    fn test_verify_ports_udp_unreachable() {
        solana_logger::setup();
        let ip_addr = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let port_range = localhost_port_range_for_tests();
        let config = SocketConfig::default();
        let (_server_port, (_server_udp_socket, server_tcp_listener)) =
            bind_common_in_range_with_config(ip_addr, port_range, config).unwrap();

        // make the socket unreachable by not running the ip echo server!
        let server_ip_echo_addr = server_tcp_listener.local_addr().unwrap();

        let (_client_port, client_udp_socket) =
            bind_in_range_with_config(ip_addr, port_range, config).unwrap();

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
        solana_logger::setup();
        let ip_addr = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let config = SocketConfig::default();
        let mut tcp_listeners = vec![];
        let mut udp_sockets = vec![];

        let (_server_port, (_, server_tcp_listener)) =
            bind_common_in_range_with_config(ip_addr, (2200, 2300), config).unwrap();
        for _ in 0..MAX_PORT_VERIFY_THREADS * 2 {
            let (_client_port, (client_udp_socket, client_tcp_listener)) =
                bind_common_in_range_with_config(
                    ip_addr,
                    (2300, 2300 + (MAX_PORT_VERIFY_THREADS * 3) as u16),
                    config,
                )
                .unwrap();
            tcp_listeners.push(client_tcp_listener);
            udp_sockets.push(client_udp_socket);
        }

        let ip_echo_server_addr = server_tcp_listener.local_addr().unwrap();

        let rt = runtime();
        let _server = rt.spawn(ip_echo_server(server_tcp_listener, Some(65535)));

        assert_eq!(
            get_public_ip_addr_with_binding(&ip_echo_server_addr, ip_addr).unwrap(),
            ip_addr
        );

        let socket_refs = udp_sockets.iter().collect_vec();
        assert!(verify_all_reachable_tcp(
            &ip_echo_server_addr,
            tcp_listeners,
        ));
        assert!(verify_all_reachable_udp(&ip_echo_server_addr, &socket_refs));
    }
}
