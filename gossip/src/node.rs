use {
    crate::{
        cluster_info::{DEFAULT_NUM_VOTOR_QUIC_ENDPOINTS, NodeConfig, Sockets},
        contact_info::{
            ContactInfo,
            Protocol::{QUIC, UDP},
        },
    },
    solana_net_utils::{
        find_available_ports_in_range,
        sockets::{
            SocketConfiguration as SocketConfig, UdpSocketBufferLimits,
            localhost_port_range_for_tests,
        },
    },
    solana_pubkey::Pubkey,
    solana_streamer::quic::DEFAULT_QUIC_ENDPOINTS,
    solana_time_utils::timestamp,
    std::{
        net::{IpAddr, Ipv4Addr, SocketAddr},
        num::NonZero,
    },
};

/// Socket configurations for different usage patterns
#[derive(Default)]
struct SocketConfigs {
    read_write: SocketConfig,
    primarily_read_quic: SocketConfig,
    primarily_write_quic: SocketConfig,
    primarily_read_udp: SocketConfig,
    primarily_write_udp: SocketConfig,
}

#[derive(Debug)]
pub struct Node {
    pub info: ContactInfo,
    pub sockets: Sockets,
}

impl Node {
    /// Creates socket configurations for different socket usage patterns.
    ///
    /// In Agave, many sockets are primarily read heavy or write heavy.
    /// QUIC sockets get a 4 MiB buffer on the unused side for control traffic
    /// (handshakes, ACKs, connection management). For UDP, "read/write" only
    /// describes buffer tuning: Agave does not send from primarily_read_udp
    /// sockets nor receive on primarily_write_udp sockets. Setting the unused
    /// side to 1 avoids increasing it; Linux still enforces a minimum. Note
    /// unlike Linux, MacOS and FreeBSD will not accept a buffer size of zero.
    /// Furthermore as UDP socket sends on MacOS and FreeBSD are unbuffered,
    /// there is no point increasing UDP socket send buffers on those platforms.
    ///
    /// NOTE: In Linux, the minimum send buffer size (SO_SNDBUF) is 2048 bytes
    /// and the minimum receive buffer size (SO_RCVBUF) is 256 bytes
    /// See: https://man7.org/linux/man-pages/man7/socket.7.html
    fn create_socket_configs() -> SocketConfigs {
        if cfg!(any(
            target_os = "linux",
            target_os = "freebsd",
            target_os = "macos"
        )) {
            const QUIC_CONTROL_TRAFFIC_BUFFER_SIZE: usize = 4 * 1024 * 1024; // 4 MiB

            let udp_socket_limits = UdpSocketBufferLimits::new();
            let recv_max = udp_socket_limits.recv_limit;
            let recv_quic_ctrl = udp_socket_limits.recv_clamp(QUIC_CONTROL_TRAFFIC_BUFFER_SIZE);
            let send_max = udp_socket_limits.send_limit;
            let send_quic_ctrl = udp_socket_limits.send_clamp(QUIC_CONTROL_TRAFFIC_BUFFER_SIZE);

            // As UDP socket sends on freebsd & macos are always
            // unbuffered, do not change the send buffer size.
            let (send_max, send_quic_ctrl) = if cfg!(target_os = "linux") {
                (send_max, send_quic_ctrl)
            } else {
                (None, None)
            };

            SocketConfigs {
                read_write: SocketConfig::default()
                    .recv_buffer_size(recv_max)
                    .send_buffer_size(send_max),
                primarily_read_quic: SocketConfig::default()
                    .recv_buffer_size(recv_max)
                    .send_buffer_size(send_quic_ctrl),
                primarily_write_quic: SocketConfig::default()
                    .recv_buffer_size(recv_quic_ctrl)
                    .send_buffer_size(send_max),
                primarily_read_udp: SocketConfig::default()
                    .recv_buffer_size(recv_max)
                    .send_buffer_size(Some(1)),
                primarily_write_udp: SocketConfig::default()
                    .recv_buffer_size(Some(1))
                    .send_buffer_size(send_max),
            }
        } else {
            SocketConfigs::default()
        }
    }

    /// create localhost node for tests
    pub fn new_localhost() -> Self {
        let pubkey = solana_pubkey::new_rand();
        Self::new_localhost_with_pubkey(&pubkey)
    }

    /// create localhost node for tests with provided pubkey
    /// unlike the [`Node::new_with_external_ip`], this will also bind RPC sockets.
    pub fn new_localhost_with_pubkey(pubkey: &Pubkey) -> Self {
        let port_range = localhost_port_range_for_tests();
        let bind_ip_addr = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let config = NodeConfig {
            bind_ip_addr,
            gossip_port: port_range.0,
            port_range,
            advertised_ip: bind_ip_addr,
            public_tpu_addr: None,
            public_tpu_forwards_addr: None,
            public_tvu_addr: None,
            num_tvu_receive_sockets: NonZero::new(1).unwrap(),
            num_tvu_retransmit_sockets: NonZero::new(1).unwrap(),
            num_quic_endpoints: NonZero::new(DEFAULT_QUIC_ENDPOINTS)
                .expect("Number of QUIC endpoints can not be zero"),
            num_votor_quic_endpoints: DEFAULT_NUM_VOTOR_QUIC_ENDPOINTS,
        };
        let mut node = Self::new_with_external_ip(pubkey, config);
        let rpc_ports: [u16; 2] = find_available_ports_in_range(bind_ip_addr, port_range).unwrap();
        let rpc_addr = SocketAddr::new(bind_ip_addr, rpc_ports[0]);
        let rpc_pubsub_addr = SocketAddr::new(bind_ip_addr, rpc_ports[1]);
        node.info.set_rpc(rpc_addr).unwrap();
        node.info.set_rpc_pubsub(rpc_pubsub_addr).unwrap();
        node
    }

    pub fn new_with_external_ip(pubkey: &Pubkey, config: NodeConfig) -> Node {
        let NodeConfig {
            advertised_ip,
            gossip_port,
            port_range,
            bind_ip_addr,
            public_tpu_addr,
            public_tpu_forwards_addr,
            public_tvu_addr,
            num_tvu_receive_sockets,
            num_tvu_retransmit_sockets,
            num_quic_endpoints,
            num_votor_quic_endpoints: num_votor_endpoints,
        } = config;
        let socket_configs = Self::create_socket_configs();

        let (gossip, ip_echo) = socket_configs
            .read_write
            .bind_udp_tcp_to_or_range(bind_ip_addr, gossip_port, port_range)
            .expect("gossip bind_udp_to_or_range");
        let gossip_port = gossip.local_addr().unwrap().port();
        let tvu_sockets = socket_configs
            .primarily_read_udp
            .multi_bind_udp_range(bind_ip_addr, port_range, num_tvu_receive_sockets.get())
            .expect("tvu multi_bind");
        let tvu_port = tvu_sockets.first().unwrap().local_addr().unwrap().port();
        let tpu_quic = socket_configs
            .primarily_read_quic
            .bind_udp_range(bind_ip_addr, port_range)
            .expect("tpu_quic primary bind");
        let tpu_port_quic = tpu_quic.local_addr().unwrap().port();
        let tpu_quic = socket_configs
            .primarily_read_quic
            .bind_udp_reuse(tpu_quic, num_quic_endpoints.get())
            .expect("tpu_quic multi_bind");

        let tpu_forwards_quic = socket_configs
            .primarily_read_quic
            .bind_udp_range(bind_ip_addr, port_range)
            .expect("tpu_forwards_quic primary bind");
        let tpu_forwards_quic_port = tpu_forwards_quic.local_addr().unwrap().port();
        let tpu_forwards_quic = socket_configs
            .primarily_read_quic
            .bind_udp_reuse(tpu_forwards_quic, num_quic_endpoints.get())
            .expect("tpu_forwards_quic multi_bind");

        let retransmit_sockets = socket_configs
            .primarily_write_udp
            .multi_bind_udp_range(bind_ip_addr, port_range, num_tvu_retransmit_sockets.get())
            .expect("tvu retransmit multi_bind");
        let repair = socket_configs
            .read_write
            .bind_udp_range(bind_ip_addr, port_range)
            .expect("repair bind");

        let serve_repair = socket_configs
            .read_write
            .bind_udp_range(bind_ip_addr, port_range)
            .expect("serve_repair");
        let serve_repair_port = serve_repair.local_addr().unwrap().port();

        let broadcast = socket_configs
            .primarily_write_udp
            .multi_bind_udp_range(bind_ip_addr, port_range, 2)
            .expect("broadcast multi_bind");

        let votor_server_primary = socket_configs
            .primarily_read_quic
            .bind_udp_range(bind_ip_addr, port_range)
            .expect("Votor server port bind should succeed");
        let votor_server_port = votor_server_primary.local_addr().unwrap().port();
        let votor_server = socket_configs
            .primarily_read_quic
            .bind_udp_reuse(votor_server_primary, num_votor_endpoints.get())
            .expect("Votor server SO_REUSEPORT bind should succeed");

        let block_id_repair = socket_configs
            .read_write
            .bind_udp_range(bind_ip_addr, port_range)
            .expect("Block ID repair port bind should succeed");

        // These are "client" sockets, so they could use ephemeral ports, but we
        // force them into the provided port_range to simplify the operations.

        let tpu_vote_forwarding_client = socket_configs
            .primarily_write_udp
            .bind_udp_range(bind_ip_addr, port_range)
            .unwrap();

        let tpu_transaction_forwarding_client = socket_configs
            .primarily_write_quic
            .bind_udp_range(bind_ip_addr, port_range)
            .expect("TPU transaction forwarding client bind should succeed");

        let quic_votor_client = socket_configs
            .primarily_write_quic
            .bind_udp_range(bind_ip_addr, port_range)
            .expect("Votor client socket bind should succeed");

        let rpc_sts_client = socket_configs
            .primarily_write_quic
            .bind_udp_range(bind_ip_addr, port_range)
            .unwrap();

        let mut info = ContactInfo::new(
            *pubkey,
            timestamp(), // wallclock
            0u16,        // shred_version
        );

        info.set_gossip((advertised_ip, gossip_port)).unwrap();
        info.set_tvu(
            UDP,
            public_tvu_addr.unwrap_or_else(|| SocketAddr::new(advertised_ip, tvu_port)),
        )
        .unwrap();
        // placeholder to prevent legacy nodes from assuming we do not have open TPU ports
        // see https://github.com/anza-xyz/agave/pull/10174
        info.set_tpu(
            UDP,
            public_tpu_addr.unwrap_or_else(|| SocketAddr::new(advertised_ip, 1)),
        )
        .unwrap();
        info.set_tpu(
            QUIC,
            public_tpu_addr.unwrap_or_else(|| SocketAddr::new(advertised_ip, tpu_port_quic)),
        )
        .unwrap();
        // placeholder to prevent legacy nodes from assuming we do not have open TPU ports
        // see https://github.com/anza-xyz/agave/pull/10174
        info.set_tpu_forwards(
            UDP,
            public_tpu_forwards_addr.unwrap_or_else(|| SocketAddr::new(advertised_ip, 1)),
        )
        .unwrap();
        info.set_tpu_forwards(
            QUIC,
            public_tpu_forwards_addr
                .unwrap_or_else(|| SocketAddr::new(advertised_ip, tpu_forwards_quic_port)),
        )
        .unwrap();
        info.set_serve_repair(UDP, (advertised_ip, serve_repair_port))
            .unwrap();
        // placeholder to prevent legacy agave nodes from assuming we do not have open repair ports
        // see https://github.com/anza-xyz/agave/pull/10460#discussion_r3054463946 for context and
        // cleanup timing.
        info.set_serve_repair(QUIC, (advertised_ip, 1)).unwrap();
        info.set_alpenglow((advertised_ip, votor_server_port))
            .unwrap();

        trace!("new ContactInfo: {info:?}");
        let sockets = Sockets {
            votor_server,
            gossip,
            tvu: tvu_sockets,
            broadcast,
            repair,
            retransmit_sockets,
            serve_repair,
            ip_echo: Some(ip_echo),
            block_id_repair,
            tpu_quic,
            tpu_forwards_quic,
            tpu_vote_forwarding_client,
            quic_votor_client,
            tpu_transaction_forwarding_client,
            rpc_sts_client,
        };
        info!("Bound all network sockets as follows: {sockets:?}");
        Node { info, sockets }
    }
}

#[cfg(test)]
mod tests {
    use {super::*, crate::contact_info::Protocol::QUIC};

    /// Regression test for fix where tpu_forwards_quic was incorrectly
    /// using tpu_forwards_port (UDP) instead of tpu_forwards_quic_port (QUIC)
    #[test]
    fn test_tpu_forwards_quic_uses_correct_port() {
        let pubkey = solana_pubkey::new_rand();
        let node = Node::new_localhost_with_pubkey(&pubkey);

        let tpu_forwards_quic = node.info.tpu_forwards(QUIC).unwrap();

        let actual_quic_port = node.sockets.tpu_forwards_quic[0]
            .local_addr()
            .unwrap()
            .port();

        assert_eq!(
            tpu_forwards_quic.port(),
            actual_quic_port,
            "TPU forwards QUIC advertised port should match actual bound QUIC socket"
        );
    }
}
