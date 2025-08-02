use {
    crate::{
        cluster_info::{BindIpAddrs, NodeConfig, Sockets},
        contact_info::{
            ContactInfo,
            Protocol::{QUIC, UDP},
        },
    },
    solana_net_utils::{
        find_available_ports_in_range,
        sockets::{
            bind_gossip_port_in_range, bind_in_range_with_config, bind_more_with_config,
            bind_to_with_config, bind_two_in_range_with_offset_and_config,
            localhost_port_range_for_tests, multi_bind_in_range_with_config,
            SocketConfiguration as SocketConfig,
        },
        PortRange,
    },
    solana_pubkey::Pubkey,
    solana_quic_definitions::QUIC_PORT_OFFSET,
    solana_streamer::{atomic_udp_socket::AtomicUdpSocket, quic::DEFAULT_QUIC_ENDPOINTS},
    solana_time_utils::timestamp,
    std::{
        net::{IpAddr, Ipv4Addr, SocketAddr},
        num::NonZero,
    },
};

#[derive(Debug)]
pub struct Node {
    pub info: ContactInfo,
    pub sockets: Sockets,
}

impl Node {
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
            bind_ip_addrs: BindIpAddrs::new(vec![bind_ip_addr]).expect("should bind"),
            gossip_port: port_range.0,
            port_range,
            advertised_ip: bind_ip_addr,
            public_tpu_addr: None,
            public_tpu_forwards_addr: None,
            num_tvu_receive_sockets: NonZero::new(1).unwrap(),
            num_tvu_retransmit_sockets: NonZero::new(1).unwrap(),
            num_quic_endpoints: NonZero::new(DEFAULT_QUIC_ENDPOINTS)
                .expect("Number of QUIC endpoints can not be zero"),
            vortexor_receiver_addr: None,
        };
        let mut node = Self::new_with_external_ip(pubkey, config);
        let rpc_ports: [u16; 2] = find_available_ports_in_range(bind_ip_addr, port_range).unwrap();
        let rpc_addr = SocketAddr::new(bind_ip_addr, rpc_ports[0]);
        let rpc_pubsub_addr = SocketAddr::new(bind_ip_addr, rpc_ports[1]);
        node.info.set_rpc(rpc_addr).unwrap();
        node.info.set_rpc_pubsub(rpc_pubsub_addr).unwrap();
        node
    }

    #[deprecated(since = "3.0.0", note = "use new_with_external_ip")]
    pub fn new_single_bind(
        pubkey: &Pubkey,
        gossip_addr: &SocketAddr,
        port_range: PortRange,
        bind_ip_addr: IpAddr,
    ) -> Self {
        let config = NodeConfig {
            bind_ip_addrs: BindIpAddrs::new(vec![bind_ip_addr]).expect("should bind"),
            gossip_port: gossip_addr.port(),
            port_range,
            advertised_ip: bind_ip_addr,
            public_tpu_addr: None,
            public_tpu_forwards_addr: None,
            num_tvu_receive_sockets: NonZero::new(1).unwrap(),
            num_tvu_retransmit_sockets: NonZero::new(1).unwrap(),
            num_quic_endpoints: NonZero::new(DEFAULT_QUIC_ENDPOINTS)
                .expect("Number of QUIC endpoints can not be zero"),
            vortexor_receiver_addr: None,
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
            bind_ip_addrs,
            public_tpu_addr,
            public_tpu_forwards_addr,
            num_tvu_receive_sockets,
            num_tvu_retransmit_sockets,
            num_quic_endpoints,
            vortexor_receiver_addr,
        } = config;
        let bind_ip_addr = bind_ip_addrs.primary();

        let gossip_addr = SocketAddr::new(advertised_ip, gossip_port);
        let (gossip_port, (gossip, ip_echo)) =
            bind_gossip_port_in_range(&gossip_addr, port_range, bind_ip_addr);

        let socket_config = SocketConfig::default();

        let (tvu_port, tvu_sockets) = multi_bind_in_range_with_config(
            bind_ip_addr,
            port_range,
            socket_config,
            num_tvu_receive_sockets.get(),
        )
        .expect("tvu multi_bind");
        let (tvu_quic_port, tvu_quic) =
            bind_in_range_with_config(bind_ip_addr, port_range, socket_config)
                .expect("tvu_quic bind");

        let ((tpu_port, tpu_socket), (_tpu_port_quic, tpu_quic)) =
            bind_two_in_range_with_offset_and_config(
                bind_ip_addr,
                port_range,
                QUIC_PORT_OFFSET,
                socket_config,
                socket_config,
            )
            .expect("tpu_socket primary bind");
        let tpu_sockets =
            bind_more_with_config(tpu_socket, 32, socket_config).expect("tpu_sockets multi_bind");

        let tpu_quic = bind_more_with_config(tpu_quic, num_quic_endpoints.get(), socket_config)
            .expect("tpu_quic bind");

        let ((tpu_forwards_port, tpu_forwards_socket), (_, tpu_forwards_quic)) =
            bind_two_in_range_with_offset_and_config(
                bind_ip_addr,
                port_range,
                QUIC_PORT_OFFSET,
                socket_config,
                socket_config,
            )
            .expect("tpu_forwards primary bind");
        let tpu_forwards_sockets = bind_more_with_config(tpu_forwards_socket, 8, socket_config)
            .expect("tpu_forwards multi_bind");
        let tpu_forwards_quic =
            bind_more_with_config(tpu_forwards_quic, num_quic_endpoints.get(), socket_config)
                .expect("tpu_forwards_quic multi_bind");

        let (tpu_vote_port, tpu_vote_sockets) =
            multi_bind_in_range_with_config(bind_ip_addr, port_range, socket_config, 1)
                .expect("tpu_vote multi_bind");

        let (tpu_vote_quic_port, tpu_vote_quic) =
            bind_in_range_with_config(bind_ip_addr, port_range, socket_config)
                .expect("tpu_vote_quic");
        let tpu_vote_quic =
            bind_more_with_config(tpu_vote_quic, num_quic_endpoints.get(), socket_config)
                .expect("tpu_vote_quic multi_bind");

        let (_, retransmit_sockets) = multi_bind_in_range_with_config(
            bind_ip_addr,
            port_range,
            socket_config,
            num_tvu_retransmit_sockets.get(),
        )
        .expect("retransmit multi_bind");

        let (_, repair) = bind_in_range_with_config(bind_ip_addr, port_range, socket_config)
            .expect("repair bind");
        let (_, repair_quic) = bind_in_range_with_config(bind_ip_addr, port_range, socket_config)
            .expect("repair_quic bind");

        let (serve_repair_port, serve_repair) =
            bind_in_range_with_config(bind_ip_addr, port_range, socket_config)
                .expect("serve_repair");
        let (serve_repair_quic_port, serve_repair_quic) =
            bind_in_range_with_config(bind_ip_addr, port_range, socket_config)
                .expect("serve_repair_quic");

        let (_, broadcast) =
            multi_bind_in_range_with_config(bind_ip_addr, port_range, socket_config, 4)
                .expect("broadcast multi_bind");

        let (_, ancestor_hashes_requests) =
            bind_in_range_with_config(bind_ip_addr, port_range, socket_config)
                .expect("ancestor_hashes_requests bind");
        let (_, ancestor_hashes_requests_quic) =
            bind_in_range_with_config(bind_ip_addr, port_range, socket_config)
                .expect("ancestor_hashes_requests QUIC bind should succeed");

        // These are client sockets, so the port is set to be 0 because it must be ephimeral.
        let tpu_vote_forwarding_client =
            bind_to_with_config(bind_ip_addr, 0, socket_config).unwrap();
        let tpu_transaction_forwarding_client =
            bind_to_with_config(bind_ip_addr, 0, socket_config).unwrap();
        let quic_vote_client = bind_to_with_config(bind_ip_addr, 0, socket_config).unwrap();
        let rpc_sts_client = bind_to_with_config(bind_ip_addr, 0, socket_config).unwrap();

        let mut info = ContactInfo::new(
            *pubkey,
            timestamp(), // wallclock
            0u16,        // shred_version
        );

        info.set_gossip((advertised_ip, gossip_port)).unwrap();
        info.set_tvu(UDP, (advertised_ip, tvu_port)).unwrap();
        info.set_tvu(QUIC, (advertised_ip, tvu_quic_port)).unwrap();
        info.set_tpu(public_tpu_addr.unwrap_or_else(|| SocketAddr::new(advertised_ip, tpu_port)))
            .unwrap();
        info.set_tpu_forwards(
            public_tpu_forwards_addr
                .unwrap_or_else(|| SocketAddr::new(advertised_ip, tpu_forwards_port)),
        )
        .unwrap();
        info.set_tpu_vote(UDP, (advertised_ip, tpu_vote_port))
            .unwrap();
        info.set_tpu_vote(QUIC, (advertised_ip, tpu_vote_quic_port))
            .unwrap();
        info.set_serve_repair(UDP, (advertised_ip, serve_repair_port))
            .unwrap();
        info.set_serve_repair(QUIC, (advertised_ip, serve_repair_quic_port))
            .unwrap();

        let vortexor_receivers = vortexor_receiver_addr.map(|vortexor_receiver_addr| {
            multi_bind_in_range_with_config(
                vortexor_receiver_addr.ip(),
                (
                    vortexor_receiver_addr.port(),
                    vortexor_receiver_addr.port() + 1,
                ),
                socket_config,
                32,
            )
            .unwrap_or_else(|_| {
                panic!("Could not bind to the set vortexor_receiver_addr {vortexor_receiver_addr}")
            })
            .1
        });

        info!("vortexor_receivers is {vortexor_receivers:?}");
        trace!("new ContactInfo: {info:?}");
        let sockets = Sockets {
            gossip: AtomicUdpSocket::new(gossip),
            tvu: tvu_sockets,
            tvu_quic,
            tpu: tpu_sockets,
            tpu_forwards: tpu_forwards_sockets,
            tpu_vote: tpu_vote_sockets,
            broadcast,
            repair,
            repair_quic,
            retransmit_sockets,
            serve_repair,
            serve_repair_quic,
            ip_echo: Some(ip_echo),
            ancestor_hashes_requests,
            ancestor_hashes_requests_quic,
            tpu_quic,
            tpu_forwards_quic,
            tpu_vote_quic,
            tpu_vote_forwarding_client,
            quic_vote_client,
            tpu_transaction_forwarding_client,
            rpc_sts_client,
            vortexor_receivers,
        };
        info!("Bound all network sockets as follows: {:#?}", &sockets);
        Node { info, sockets }
    }
}
