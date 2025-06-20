use crate::cluster_info::Sockets;
use crate::contact_info;
use crate::contact_info::ContactInfo;
use solana_net_utils::SocketConfig;
use solana_net_utils::{
    bind_common_in_range_with_config, bind_in_range_with_config, bind_more_with_config,
    bind_to_localhost, bind_to_unspecified, bind_to_with_config,
    bind_two_in_range_with_offset_and_config, find_available_ports_in_range,
    multi_bind_in_range_with_config,
    sockets::{bind_gossip_port_in_range, localhost_port_range_for_tests},
    PortRange,
};
use solana_pubkey::Pubkey;
use solana_quic_definitions::QUIC_PORT_OFFSET;
use solana_streamer::quic::DEFAULT_QUIC_ENDPOINTS;
use solana_time_utils::timestamp;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::num::NonZeroUsize;

#[derive(Debug)]
pub struct Node {
    pub info: ContactInfo,
    pub sockets: Sockets,
}

pub struct NodeConfig {
    pub gossip_addr: SocketAddr,
    pub port_range: PortRange,
    pub bind_ip_addr: IpAddr,
    pub public_tpu_addr: Option<SocketAddr>,
    pub public_tpu_forwards_addr: Option<SocketAddr>,
    /// The number of TVU receive sockets to create
    pub num_tvu_receive_sockets: NonZeroUsize,
    /// The number of TVU retransmit sockets to create
    pub num_tvu_retransmit_sockets: NonZeroUsize,
    /// The number of QUIC tpu endpoints
    pub num_quic_endpoints: NonZeroUsize,
}

impl Node {
    pub fn new_localhost() -> Self {
        let pubkey = solana_pubkey::new_rand();
        Self::new_localhost_with_pubkey(&pubkey)
    }

    pub fn new_localhost_with_pubkey(pubkey: &Pubkey) -> Self {
        Self::new_localhost_with_pubkey_and_quic_endpoints(pubkey, DEFAULT_QUIC_ENDPOINTS)
    }

    pub fn new_localhost_with_pubkey_and_quic_endpoints(
        pubkey: &Pubkey,
        num_quic_endpoints: usize,
    ) -> Self {
        let localhost_ip_addr = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let port_range = localhost_port_range_for_tests();
        let udp_config = SocketConfig::default();
        let quic_config = SocketConfig::default().reuseport(true);
        let ((_tpu_port, tpu), (_tpu_quic_port, tpu_quic)) =
            bind_two_in_range_with_offset_and_config(
                localhost_ip_addr,
                port_range,
                QUIC_PORT_OFFSET,
                udp_config,
                quic_config,
            )
            .unwrap();
        let tpu_quic = bind_more_with_config(tpu_quic, num_quic_endpoints, quic_config).unwrap();
        let (gossip_port, (gossip, ip_echo)) =
            bind_common_in_range_with_config(localhost_ip_addr, port_range, udp_config).unwrap();
        let gossip_addr = SocketAddr::new(localhost_ip_addr, gossip_port);
        let tvu = bind_to_localhost().unwrap();
        let tvu_quic = bind_to_localhost().unwrap();
        let ((_tpu_forwards_port, tpu_forwards), (_tpu_forwards_quic_port, tpu_forwards_quic)) =
            bind_two_in_range_with_offset_and_config(
                localhost_ip_addr,
                port_range,
                QUIC_PORT_OFFSET,
                udp_config,
                quic_config,
            )
            .unwrap();
        let tpu_forwards_quic =
            bind_more_with_config(tpu_forwards_quic, num_quic_endpoints, quic_config).unwrap();
        let tpu_vote = bind_to_localhost().unwrap();
        let tpu_vote_quic = bind_to_localhost().unwrap();
        let tpu_vote_quic =
            bind_more_with_config(tpu_vote_quic, num_quic_endpoints, quic_config).unwrap();

        let repair = bind_to_localhost().unwrap();
        let repair_quic = bind_to_localhost().unwrap();
        let [rpc_port, rpc_pubsub_port] =
            find_available_ports_in_range(localhost_ip_addr, port_range).unwrap();
        let rpc_addr = SocketAddr::new(localhost_ip_addr, rpc_port);
        let rpc_pubsub_addr = SocketAddr::new(localhost_ip_addr, rpc_pubsub_port);
        let broadcast = vec![bind_to_unspecified().unwrap()];
        let retransmit_socket = bind_to_unspecified().unwrap();
        let serve_repair = bind_to_localhost().unwrap();
        let serve_repair_quic = bind_to_localhost().unwrap();
        let ancestor_hashes_requests = bind_to_unspecified().unwrap();
        let ancestor_hashes_requests_quic = bind_to_unspecified().unwrap();

        let tpu_vote_forwarding_client = bind_to_localhost().unwrap();
        let tpu_transaction_forwarding_client = bind_to_localhost().unwrap();
        let quic_vote_client = bind_to_localhost().unwrap();
        let rpc_sts_client = bind_to_localhost().unwrap();

        let mut info = ContactInfo::new(
            *pubkey,
            timestamp(), // wallclock
            0u16,        // shred_version
        );
        macro_rules! set_socket {
            ($method:ident, $addr:expr, $name:literal) => {
                info.$method($addr).expect(&format!(
                    "Operator must spin up node with valid {} address",
                    $name
                ))
            };
            ($method:ident, $protocol:ident, $addr:expr, $name:literal) => {{
                info.$method(contact_info::Protocol::$protocol, $addr)
                    .expect(&format!(
                        "Operator must spin up node with valid {} address",
                        $name
                    ))
            }};
        }
        set_socket!(set_gossip, gossip_addr, "gossip");
        set_socket!(set_tvu, UDP, tvu.local_addr().unwrap(), "TVU");
        set_socket!(set_tvu, QUIC, tvu_quic.local_addr().unwrap(), "TVU QUIC");
        set_socket!(set_tpu, tpu.local_addr().unwrap(), "TPU");
        set_socket!(
            set_tpu_forwards,
            tpu_forwards.local_addr().unwrap(),
            "TPU-forwards"
        );
        set_socket!(
            set_tpu_vote,
            UDP,
            tpu_vote.local_addr().unwrap(),
            "TPU-vote"
        );
        set_socket!(
            set_tpu_vote,
            QUIC,
            tpu_vote_quic[0].local_addr().unwrap(),
            "TPU-vote QUIC"
        );
        set_socket!(set_rpc, rpc_addr, "RPC");
        set_socket!(set_rpc_pubsub, rpc_pubsub_addr, "RPC-pubsub");
        set_socket!(
            set_serve_repair,
            UDP,
            serve_repair.local_addr().unwrap(),
            "serve-repair"
        );
        set_socket!(
            set_serve_repair,
            QUIC,
            serve_repair_quic.local_addr().unwrap(),
            "serve-repair QUIC"
        );
        Node {
            info,
            sockets: Sockets {
                gossip,
                ip_echo: Some(ip_echo),
                tvu: vec![tvu],
                tvu_quic,
                tpu: vec![tpu],
                tpu_forwards: vec![tpu_forwards],
                tpu_vote: vec![tpu_vote],
                broadcast,
                repair,
                repair_quic,
                retransmit_sockets: vec![retransmit_socket],
                serve_repair,
                serve_repair_quic,
                ancestor_hashes_requests,
                ancestor_hashes_requests_quic,
                tpu_quic,
                tpu_forwards_quic,
                tpu_vote_quic,
                tpu_vote_forwarding_client,
                tpu_transaction_forwarding_client,
                quic_vote_client,
                rpc_sts_client,
            },
        }
    }

    fn bind_with_config(
        bind_ip_addr: IpAddr,
        port_range: PortRange,
        config: SocketConfig,
    ) -> (u16, UdpSocket) {
        bind_in_range_with_config(bind_ip_addr, port_range, config).expect("Failed to bind")
    }

    pub fn new_single_bind(
        pubkey: &Pubkey,
        gossip_addr: &SocketAddr,
        port_range: PortRange,
        bind_ip_addr: IpAddr,
    ) -> Self {
        let (gossip_port, (gossip, ip_echo)) =
            bind_gossip_port_in_range(gossip_addr, port_range, bind_ip_addr);

        let socket_config = SocketConfig::default();
        let socket_config_reuseport = SocketConfig::default().reuseport(true);
        let (tvu_port, tvu) = Self::bind_with_config(bind_ip_addr, port_range, socket_config);
        let (tvu_quic_port, tvu_quic) =
            Self::bind_with_config(bind_ip_addr, port_range, socket_config);
        let ((tpu_port, tpu), (_tpu_quic_port, tpu_quic)) =
            bind_two_in_range_with_offset_and_config(
                bind_ip_addr,
                port_range,
                QUIC_PORT_OFFSET,
                socket_config,
                socket_config_reuseport,
            )
            .unwrap();
        let tpu_quic: Vec<UdpSocket> =
            bind_more_with_config(tpu_quic, DEFAULT_QUIC_ENDPOINTS, socket_config_reuseport)
                .unwrap();

        let ((tpu_forwards_port, tpu_forwards), (_tpu_forwards_quic_port, tpu_forwards_quic)) =
            bind_two_in_range_with_offset_and_config(
                bind_ip_addr,
                port_range,
                QUIC_PORT_OFFSET,
                socket_config,
                socket_config_reuseport,
            )
            .unwrap();
        let tpu_forwards_quic = bind_more_with_config(
            tpu_forwards_quic,
            DEFAULT_QUIC_ENDPOINTS,
            socket_config_reuseport,
        )
        .unwrap();

        let (tpu_vote_port, tpu_vote) =
            Self::bind_with_config(bind_ip_addr, port_range, socket_config);
        let (tpu_vote_quic_port, tpu_vote_quic) =
            Self::bind_with_config(bind_ip_addr, port_range, socket_config);
        let tpu_vote_quic: Vec<UdpSocket> = bind_more_with_config(
            tpu_vote_quic,
            DEFAULT_QUIC_ENDPOINTS,
            socket_config_reuseport,
        )
        .unwrap();

        let (_, retransmit_socket) =
            Self::bind_with_config(bind_ip_addr, port_range, socket_config);
        let (_, repair) = Self::bind_with_config(bind_ip_addr, port_range, socket_config);
        let (_, repair_quic) = Self::bind_with_config(bind_ip_addr, port_range, socket_config);
        let (serve_repair_port, serve_repair) =
            Self::bind_with_config(bind_ip_addr, port_range, socket_config);
        let (serve_repair_quic_port, serve_repair_quic) =
            Self::bind_with_config(bind_ip_addr, port_range, socket_config);
        let (_, broadcast) = Self::bind_with_config(bind_ip_addr, port_range, socket_config);
        let (_, ancestor_hashes_requests) =
            Self::bind_with_config(bind_ip_addr, port_range, socket_config);
        let (_, ancestor_hashes_requests_quic) =
            Self::bind_with_config(bind_ip_addr, port_range, socket_config);

        let [rpc_port, rpc_pubsub_port] =
            find_available_ports_in_range(bind_ip_addr, port_range).unwrap();

        // These are client sockets, so the port is set to be 0 because it must be ephimeral.
        let tpu_vote_forwarding_client =
            bind_to_with_config(bind_ip_addr, 0, socket_config).unwrap();
        let tpu_transaction_forwarding_client =
            bind_to_with_config(bind_ip_addr, 0, socket_config).unwrap();
        let quic_vote_client = bind_to_with_config(bind_ip_addr, 0, socket_config).unwrap();
        let rpc_sts_client = bind_to_with_config(bind_ip_addr, 0, socket_config).unwrap();

        let addr = gossip_addr.ip();
        let mut info = ContactInfo::new(
            *pubkey,
            timestamp(), // wallclock
            0u16,        // shred_version
        );
        macro_rules! set_socket {
            ($method:ident, $port:ident, $name:literal) => {
                info.$method((addr, $port)).expect(&format!(
                    "Operator must spin up node with valid {} address",
                    $name
                ))
            };
            ($method:ident, $protocol:ident, $port:ident, $name:literal) => {{
                info.$method(contact_info::Protocol::$protocol, (addr, $port))
                    .expect(&format!(
                        "Operator must spin up node with valid {} address",
                        $name
                    ))
            }};
        }
        set_socket!(set_gossip, gossip_port, "gossip");
        set_socket!(set_tvu, UDP, tvu_port, "TVU");
        set_socket!(set_tvu, QUIC, tvu_quic_port, "TVU QUIC");
        set_socket!(set_tpu, tpu_port, "TPU");
        set_socket!(set_tpu_forwards, tpu_forwards_port, "TPU-forwards");
        set_socket!(set_tpu_vote, UDP, tpu_vote_port, "TPU-vote");
        set_socket!(set_tpu_vote, QUIC, tpu_vote_quic_port, "TPU-vote QUIC");
        set_socket!(set_rpc, rpc_port, "RPC");
        set_socket!(set_rpc_pubsub, rpc_pubsub_port, "RPC-pubsub");
        set_socket!(set_serve_repair, UDP, serve_repair_port, "serve-repair");
        set_socket!(
            set_serve_repair,
            QUIC,
            serve_repair_quic_port,
            "serve-repair QUIC"
        );

        trace!("new ContactInfo: {:?}", info);

        Node {
            info,
            sockets: Sockets {
                gossip,
                ip_echo: Some(ip_echo),
                tvu: vec![tvu],
                tvu_quic,
                tpu: vec![tpu],
                tpu_forwards: vec![tpu_forwards],
                tpu_vote: vec![tpu_vote],
                broadcast: vec![broadcast],
                repair,
                repair_quic,
                retransmit_sockets: vec![retransmit_socket],
                serve_repair,
                serve_repair_quic,
                ancestor_hashes_requests,
                ancestor_hashes_requests_quic,
                tpu_quic,
                tpu_forwards_quic,
                tpu_vote_quic,
                tpu_vote_forwarding_client,
                quic_vote_client,
                tpu_transaction_forwarding_client,
                rpc_sts_client,
            },
        }
    }

    pub fn new_with_external_ip(pubkey: &Pubkey, config: NodeConfig) -> Node {
        let NodeConfig {
            gossip_addr,
            port_range,
            bind_ip_addr,
            public_tpu_addr,
            public_tpu_forwards_addr,
            num_tvu_receive_sockets,
            num_tvu_retransmit_sockets,
            num_quic_endpoints,
        } = config;

        let (gossip_port, (gossip, ip_echo)) =
            bind_gossip_port_in_range(&gossip_addr, port_range, bind_ip_addr);

        let socket_config = SocketConfig::default();
        let socket_config_reuseport = SocketConfig::default().reuseport(true);

        let (tvu_port, tvu_sockets) = multi_bind_in_range_with_config(
            bind_ip_addr,
            port_range,
            socket_config_reuseport,
            num_tvu_receive_sockets.get(),
        )
        .expect("tvu multi_bind");

        let (tvu_quic_port, tvu_quic) =
            Self::bind_with_config(bind_ip_addr, port_range, socket_config);

        let (tpu_port, tpu_sockets) =
            multi_bind_in_range_with_config(bind_ip_addr, port_range, socket_config_reuseport, 32)
                .expect("tpu multi_bind");

        let (_tpu_port_quic, tpu_quic) = Self::bind_with_config(
            bind_ip_addr,
            (tpu_port + QUIC_PORT_OFFSET, tpu_port + QUIC_PORT_OFFSET + 1),
            socket_config_reuseport,
        );
        let tpu_quic =
            bind_more_with_config(tpu_quic, num_quic_endpoints.get(), socket_config_reuseport)
                .unwrap();

        let (tpu_forwards_port, tpu_forwards_sockets) =
            multi_bind_in_range_with_config(bind_ip_addr, port_range, socket_config_reuseport, 8)
                .expect("tpu_forwards multi_bind");

        let (_tpu_forwards_port_quic, tpu_forwards_quic) = Self::bind_with_config(
            bind_ip_addr,
            (
                tpu_forwards_port + QUIC_PORT_OFFSET,
                tpu_forwards_port + QUIC_PORT_OFFSET + 1,
            ),
            socket_config_reuseport,
        );
        let tpu_forwards_quic = bind_more_with_config(
            tpu_forwards_quic,
            num_quic_endpoints.get(),
            socket_config_reuseport,
        )
        .unwrap();

        let (tpu_vote_port, tpu_vote_sockets) =
            multi_bind_in_range_with_config(bind_ip_addr, port_range, socket_config_reuseport, 1)
                .expect("tpu_vote multi_bind");

        let (tpu_vote_quic_port, tpu_vote_quic) =
            Self::bind_with_config(bind_ip_addr, port_range, socket_config);

        let tpu_vote_quic = bind_more_with_config(
            tpu_vote_quic,
            num_quic_endpoints.get(),
            socket_config_reuseport,
        )
        .unwrap();

        let (_, retransmit_sockets) = multi_bind_in_range_with_config(
            bind_ip_addr,
            port_range,
            socket_config_reuseport,
            num_tvu_retransmit_sockets.get(),
        )
        .expect("retransmit multi_bind");

        let (_, repair) = Self::bind_with_config(bind_ip_addr, port_range, socket_config);
        let (_, repair_quic) = Self::bind_with_config(bind_ip_addr, port_range, socket_config);

        let (serve_repair_port, serve_repair) =
            Self::bind_with_config(bind_ip_addr, port_range, socket_config);
        let (serve_repair_quic_port, serve_repair_quic) =
            Self::bind_with_config(bind_ip_addr, port_range, socket_config);

        let (_, broadcast) =
            multi_bind_in_range_with_config(bind_ip_addr, port_range, socket_config_reuseport, 4)
                .expect("broadcast multi_bind");

        let (_, ancestor_hashes_requests) =
            Self::bind_with_config(bind_ip_addr, port_range, socket_config);
        let (_, ancestor_hashes_requests_quic) =
            Self::bind_with_config(bind_ip_addr, port_range, socket_config);

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
        let addr = gossip_addr.ip();
        use contact_info::Protocol::{QUIC, UDP};
        info.set_gossip((addr, gossip_port)).unwrap();
        info.set_tvu(UDP, (addr, tvu_port)).unwrap();
        info.set_tvu(QUIC, (addr, tvu_quic_port)).unwrap();
        info.set_tpu(public_tpu_addr.unwrap_or_else(|| SocketAddr::new(addr, tpu_port)))
            .unwrap();
        info.set_tpu_forwards(
            public_tpu_forwards_addr.unwrap_or_else(|| SocketAddr::new(addr, tpu_forwards_port)),
        )
        .unwrap();
        info.set_tpu_vote(UDP, (addr, tpu_vote_port)).unwrap();
        info.set_tpu_vote(QUIC, (addr, tpu_vote_quic_port)).unwrap();
        info.set_serve_repair(UDP, (addr, serve_repair_port))
            .unwrap();
        info.set_serve_repair(QUIC, (addr, serve_repair_quic_port))
            .unwrap();

        trace!("new ContactInfo: {:?}", info);
        let sockets = Sockets {
            gossip,
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
        };
        info!("Bound all network sockets as follows: {:#?}", &sockets);
        Node { info, sockets }
    }
}
