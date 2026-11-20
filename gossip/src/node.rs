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
            SocketConfiguration as SocketConfig, bind_gossip_port_in_range,
            bind_in_range_with_config, bind_more_with_config, localhost_port_range_for_tests,
            multi_bind_in_range_with_config,
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
    /// side to 0 avoids increasing it; Linux still enforces a minimum.
    ///
    /// NOTE: In Linux, the minimum send buffer size (SO_SNDBUF) is 2048 bytes
    /// and the minimum receive buffer size (SO_RCVBUF) is 256 bytes
    /// See: https://man7.org/linux/man-pages/man7/socket.7.html
    fn create_socket_configs() -> SocketConfigs {
        if cfg!(target_os = "linux") {
            const QUIC_CONTROL_TRAFFIC_BUFFER_SIZE: usize = 4 * 1024 * 1024; // 4 MiB
            SocketConfigs {
                read_write: SocketConfig::default(),
                primarily_read_quic: SocketConfig::default()
                    .send_buffer_size(QUIC_CONTROL_TRAFFIC_BUFFER_SIZE),
                primarily_write_quic: SocketConfig::default()
                    .recv_buffer_size(QUIC_CONTROL_TRAFFIC_BUFFER_SIZE),
                primarily_read_udp: SocketConfig::default().send_buffer_size(0),
                primarily_write_udp: SocketConfig::default().recv_buffer_size(0),
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
    /// unlike the [new_with_external_ip], this will also bind RPC sockets.
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
        let gossip_addr = SocketAddr::new(bind_ip_addr, gossip_port);
        let (gossip_port, (gossip, ip_echo)) =
            bind_gossip_port_in_range(&gossip_addr, port_range, bind_ip_addr);

        let socket_configs = Self::create_socket_configs();

        let (tvu_port, tvu_sockets) = multi_bind_in_range_with_config(
            bind_ip_addr,
            port_range,
            socket_configs.primarily_read_udp,
            num_tvu_receive_sockets.get(),
        )
        .expect("tvu multi_bind");
        let (tpu_port_quic, tpu_quic) =
            bind_in_range_with_config(bind_ip_addr, port_range, socket_configs.primarily_read_quic)
                .expect("tpu_quic primary bind");
        let tpu_quic = bind_more_with_config(
            tpu_quic,
            num_quic_endpoints.get(),
            socket_configs.primarily_read_quic,
        )
        .expect("tpu_quic multi_bind");

        let (tpu_forwards_quic_port, tpu_forwards_quic) =
            bind_in_range_with_config(bind_ip_addr, port_range, socket_configs.primarily_read_quic)
                .expect("tpu_forwards_quic primary bind");
        let tpu_forwards_quic = bind_more_with_config(
            tpu_forwards_quic,
            num_quic_endpoints.get(),
            socket_configs.primarily_read_quic,
        )
        .expect("tpu_forwards_quic multi_bind");

        let (_, retransmit_sockets) = multi_bind_in_range_with_config(
            bind_ip_addr,
            port_range,
            socket_configs.primarily_write_udp,
            num_tvu_retransmit_sockets.get(),
        )
        .expect("tvu retransmit multi_bind");
        let (_, repair) =
            bind_in_range_with_config(bind_ip_addr, port_range, socket_configs.read_write)
                .expect("repair bind");

        let (serve_repair_port, serve_repair) =
            bind_in_range_with_config(bind_ip_addr, port_range, socket_configs.read_write)
                .expect("serve_repair");

        let (_, broadcast) = multi_bind_in_range_with_config(
            bind_ip_addr,
            port_range,
            socket_configs.primarily_write_udp,
            2,
        )
        .expect("broadcast multi_bind");
        let (_, ancestor_hashes_requests) =
            bind_in_range_with_config(bind_ip_addr, port_range, socket_configs.read_write)
                .expect("ancestor_hashes_requests bind");

        let (votor_server_port, votor_server_primary) =
            bind_in_range_with_config(bind_ip_addr, port_range, socket_configs.primarily_read_quic)
                .expect("Votor server port bind should succeed");
        let votor_server = bind_more_with_config(
            votor_server_primary,
            num_votor_endpoints.get(),
            socket_configs.primarily_read_quic,
        )
        .expect("Votor server SO_REUSEPORT bind should succeed");

        let (_, block_id_repair) =
            bind_in_range_with_config(bind_ip_addr, port_range, socket_configs.read_write)
                .expect("Block ID repair port bind should succeed");

        // These are "client" sockets, so they could use ephemeral ports, but we
        // force them into the provided port_range to simplify the operations.

        let (_, tpu_vote_forwarding_client) =
            bind_in_range_with_config(bind_ip_addr, port_range, socket_configs.primarily_write_udp)
                .unwrap();

        let (_, tpu_transaction_forwarding_client) = bind_in_range_with_config(
            bind_ip_addr,
            port_range,
            socket_configs.primarily_write_quic,
        )
        .expect("TPU transaction forwarding client bind should succeed");

        let (_, quic_votor_client) = bind_in_range_with_config(
            bind_ip_addr,
            port_range,
            socket_configs.primarily_write_quic,
        )
        .expect("Votor client socket bind should succeed");

        let (_, rpc_sts_client) = bind_in_range_with_config(
            bind_ip_addr,
            port_range,
            socket_configs.primarily_write_quic,
        )
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
            tpu_vote: vec![],
            broadcast,
            repair,
            retransmit_sockets,
            serve_repair,
            ip_echo: Some(ip_echo),
            ancestor_hashes_requests,
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
