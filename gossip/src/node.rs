use {
    crate::{
        cluster_info::{NodeConfig, Sockets},
        contact_info::{
            ContactInfo,
            Protocol::{QUIC, UDP},
        },
    },
    solana_net_utils::{
        find_available_ports_in_range,
        multihomed_sockets::BindIpAddrs,
        sockets::{
            bind_gossip_port_in_range, bind_in_range_with_config, bind_more_with_config,
            bind_to_with_config, localhost_port_range_for_tests, multi_bind_in_range_with_config,
            SocketConfiguration as SocketConfig,
        },
    },
    solana_pubkey::Pubkey,
    solana_streamer::quic::DEFAULT_QUIC_ENDPOINTS,
    solana_time_utils::timestamp,
    std::{
        io,
        iter::once,
        net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
        num::NonZero,
        sync::Arc,
    },
};

// Socket addresses for each protocol across all interfaces
#[derive(Debug, Clone)]
pub struct MultihomingAddresses {
    pub tvu: Box<[SocketAddr]>,
    pub tpu_vote: Box<[SocketAddr]>,
    pub tpu_quic: Box<[SocketAddr]>,
    pub tpu_forwards_quic: Box<[SocketAddr]>,
    pub tpu_vote_quic: Box<[SocketAddr]>,
}

#[derive(Debug)]
pub struct Node {
    pub info: ContactInfo,
    pub sockets: Sockets,
    pub bind_ip_addrs: Arc<BindIpAddrs>,
    pub addresses: MultihomingAddresses,
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
            public_tvu_addr: None,
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
            public_tvu_addr,
            num_tvu_receive_sockets,
            num_tvu_retransmit_sockets,
            num_quic_endpoints,
            vortexor_receiver_addr,
        } = config;
        let bind_ip_addr = bind_ip_addrs.active();

        let mut gossip_sockets = Vec::with_capacity(bind_ip_addrs.len());
        let mut gossip_ports = Vec::with_capacity(bind_ip_addrs.len());
        let mut ip_echo_sockets = Vec::with_capacity(bind_ip_addrs.len());
        for ip in bind_ip_addrs.iter() {
            let gossip_addr = SocketAddr::new(*ip, gossip_port);
            let (port, (gossip, ip_echo)) =
                bind_gossip_port_in_range(&gossip_addr, port_range, *ip);
            gossip_sockets.push(gossip);
            gossip_ports.push(port);
            ip_echo_sockets.push(ip_echo);
        }
        let socket_config = SocketConfig::default();

        let (tvu_port, mut tvu_sockets) = multi_bind_in_range_with_config(
            bind_ip_addr,
            port_range,
            socket_config,
            num_tvu_receive_sockets.get(),
        )
        .expect("tvu multi_bind");
        // Multihoming RX for TVU
        tvu_sockets.append(
            &mut Self::bind_to_extra_ip(
                &bind_ip_addrs,
                tvu_port,
                num_tvu_receive_sockets.get(),
                socket_config,
            )
            .expect("Secondary bind TVU"),
        );
        let tvu_addresses = Self::get_socket_addrs(&tvu_sockets);

        let (tvu_quic_port, tvu_quic) =
            bind_in_range_with_config(bind_ip_addr, port_range, socket_config)
                .expect("tvu_quic bind");

        let (tpu_port, tpu_socket) =
            bind_in_range_with_config(bind_ip_addr, port_range, socket_config)
                .expect("tpu_socket primary bind");
        let tpu_sockets =
            bind_more_with_config(tpu_socket, 32, socket_config).expect("tpu_sockets multi_bind");

        let (tpu_port_quic, tpu_quic) =
            bind_in_range_with_config(bind_ip_addr, port_range, socket_config)
                .expect("tpu_socket primary bind");
        let mut tpu_quic = bind_more_with_config(tpu_quic, num_quic_endpoints.get(), socket_config)
            .expect("tpu_quic bind");

        // multihoming RX for TPU
        tpu_quic.append(
            &mut Self::bind_to_extra_ip(&bind_ip_addrs, tpu_port_quic, 32, socket_config)
                .expect("Secondary bind TPU QUIC"),
        );
        let tpu_quic_addresses = Self::get_socket_addrs(&tpu_quic);

        let (tpu_forwards_port, tpu_forwards_socket) =
            bind_in_range_with_config(bind_ip_addr, port_range, socket_config)
                .expect("tpu_forwards primary bind");
        let tpu_forwards_sockets = bind_more_with_config(tpu_forwards_socket, 8, socket_config)
            .expect("tpu_forwards multi_bind");
        let (tpu_forwards_quic_port, tpu_forwards_quic) =
            bind_in_range_with_config(bind_ip_addr, port_range, socket_config)
                .expect("tpu_forwards primary bind");
        let mut tpu_forwards_quic =
            bind_more_with_config(tpu_forwards_quic, num_quic_endpoints.get(), socket_config)
                .expect("tpu_forwards_quic multi_bind");

        tpu_forwards_quic.append(
            &mut Self::bind_to_extra_ip(
                &bind_ip_addrs,
                tpu_forwards_quic_port,
                num_quic_endpoints.get(),
                socket_config,
            )
            .expect("Secondary bind TPU forwards"),
        );
        let tpu_forwards_quic_addresses = Self::get_socket_addrs(&tpu_forwards_quic);

        let (tpu_vote_port, mut tpu_vote_sockets) =
            multi_bind_in_range_with_config(bind_ip_addr, port_range, socket_config, 1)
                .expect("tpu_vote multi_bind");

        tpu_vote_sockets.extend(
            Self::bind_to_extra_ip(&bind_ip_addrs, tpu_vote_port, 1, socket_config)
                .expect("Secondary binds for tpu vote"),
        );
        let tpu_vote_addresses = Self::get_socket_addrs(&tpu_vote_sockets);

        let (tpu_vote_quic_port, tpu_vote_quic) =
            bind_in_range_with_config(bind_ip_addr, port_range, socket_config)
                .expect("tpu_vote_quic");
        let mut tpu_vote_quic =
            bind_more_with_config(tpu_vote_quic, num_quic_endpoints.get(), socket_config)
                .expect("tpu_vote_quic multi_bind");
        tpu_vote_quic.append(
            &mut Self::bind_to_extra_ip(
                &bind_ip_addrs,
                tpu_vote_quic_port,
                num_quic_endpoints.get(),
                socket_config,
            )
            .expect("Secondary bind TPU vote"),
        );
        let tpu_vote_quic_addresses = Self::get_socket_addrs(&tpu_vote_quic);

        let (tvu_retransmit_port, mut retransmit_sockets) = multi_bind_in_range_with_config(
            bind_ip_addr,
            port_range,
            socket_config,
            num_tvu_retransmit_sockets.get(),
        )
        .expect("tvu retransmit multi_bind");
        // Multihoming TX for TVU
        retransmit_sockets.append(
            &mut Self::bind_to_extra_ip(
                &bind_ip_addrs,
                tvu_retransmit_port,
                num_tvu_retransmit_sockets.get(),
                socket_config,
            )
            .expect("Secondary bind TVU retransmit"),
        );

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

        let (broadcast_port, mut broadcast) =
            multi_bind_in_range_with_config(bind_ip_addr, port_range, socket_config, 4)
                .expect("broadcast multi_bind");
        // Multihoming TX for broadcast
        broadcast.append(
            &mut Self::bind_to_extra_ip(&bind_ip_addrs, broadcast_port, 4, socket_config)
                .expect("Secondary bind broadcast"),
        );

        let (_, ancestor_hashes_requests) =
            bind_in_range_with_config(bind_ip_addr, port_range, socket_config)
                .expect("ancestor_hashes_requests bind");
        let (_, ancestor_hashes_requests_quic) =
            bind_in_range_with_config(bind_ip_addr, port_range, socket_config)
                .expect("ancestor_hashes_requests QUIC bind should succeed");

        let (alpenglow_port, alpenglow) =
            bind_in_range_with_config(bind_ip_addr, port_range, socket_config)
                .expect("Alpenglow port bind should succeed");
        // These are "client" sockets, so they could use ephemeral ports, but we
        // force them into the provided port_range to simplify the operations.

        // vote forwarding is only bound to primary interface for now
        let (_, tpu_vote_forwarding_client) =
            bind_in_range_with_config(bind_ip_addr, port_range, socket_config).unwrap();

        let (tpu_transaction_forwarding_client_port, tpu_transaction_forwarding_clients) =
            bind_in_range_with_config(bind_ip_addr, port_range, socket_config).expect(
                "TPU transaction forwarding client bind on interface {bind_ip_addr} should succeed",
            );
        let tpu_transaction_forwarding_clients = once(tpu_transaction_forwarding_clients)
            .chain(
                Self::bind_to_extra_ip(
                    &bind_ip_addrs,
                    tpu_transaction_forwarding_client_port,
                    1,
                    socket_config,
                )
                .expect("Secondary interface binds for tpu forward clients should succeed"),
            )
            .collect();

        let (_, quic_vote_client) =
            bind_in_range_with_config(bind_ip_addr, port_range, socket_config).unwrap();

        let (_, quic_alpenglow_client) =
            bind_in_range_with_config(bind_ip_addr, port_range, socket_config).unwrap();

        let (_, rpc_sts_client) =
            bind_in_range_with_config(bind_ip_addr, port_range, socket_config).unwrap();

        let mut info = ContactInfo::new(
            *pubkey,
            timestamp(), // wallclock
            0u16,        // shred_version
        );

        info.set_gossip((advertised_ip, gossip_ports[0])).unwrap();
        info.set_tvu(
            UDP,
            public_tvu_addr.unwrap_or_else(|| SocketAddr::new(advertised_ip, tvu_port)),
        )
        .unwrap();
        info.set_tvu(QUIC, (advertised_ip, tvu_quic_port)).unwrap();
        info.set_tpu(UDP, (advertised_ip, tpu_port)).unwrap();
        info.set_tpu(
            QUIC,
            public_tpu_addr.unwrap_or_else(|| SocketAddr::new(advertised_ip, tpu_port_quic)),
        )
        .unwrap();
        info.set_tpu_forwards(UDP, (advertised_ip, tpu_forwards_port))
            .unwrap();
        info.set_tpu_forwards(
            QUIC,
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
        info.set_alpenglow((advertised_ip, alpenglow_port)).unwrap();
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
            alpenglow: Some(alpenglow),
            gossip: gossip_sockets.into_iter().collect(),
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
            ip_echo: ip_echo_sockets.into_iter().next(),
            ancestor_hashes_requests,
            ancestor_hashes_requests_quic,
            tpu_quic,
            tpu_forwards_quic,
            tpu_vote_quic,
            tpu_vote_forwarding_client,
            quic_vote_client,
            quic_alpenglow_client,
            tpu_transaction_forwarding_clients,
            rpc_sts_client,
            vortexor_receivers,
        };
        info!("Bound all network sockets as follows: {:#?}", &sockets);
        Node {
            info,
            sockets,
            bind_ip_addrs: Arc::new(bind_ip_addrs),
            addresses: MultihomingAddresses {
                tvu: tvu_addresses,
                tpu_vote: tpu_vote_addresses,
                tpu_quic: tpu_quic_addresses,
                tpu_forwards_quic: tpu_forwards_quic_addresses,
                tpu_vote_quic: tpu_vote_quic_addresses,
            },
        }
    }

    /// Extract unique addresses from bound sockets
    fn get_socket_addrs(sockets: &[UdpSocket]) -> Box<[SocketAddr]> {
        let mut addresses = Vec::new();
        let mut seen = std::collections::HashSet::new();

        for socket in sockets {
            let addr = socket.local_addr().unwrap();
            if seen.insert(addr) {
                addresses.push(addr);
            }
        }
        addresses.into()
    }

    /// Binds num sockets to each of the addresses in bind_ip_addrs except primary_ip_addr
    fn bind_to_extra_ip(
        bind_ip_addrs: &BindIpAddrs,
        port: u16,
        num: usize,
        socket_config: SocketConfig,
    ) -> io::Result<Vec<UdpSocket>> {
        let active_ip_addr = bind_ip_addrs.active();
        let mut sockets = vec![];
        for ip_addr in bind_ip_addrs
            .iter()
            .cloned()
            .filter(|&ip| ip != active_ip_addr)
        {
            let socket = bind_to_with_config(ip_addr, port, socket_config)?;
            sockets.append(&mut bind_more_with_config(socket, num, socket_config)?);
        }
        Ok(sockets)
    }
}

mod multihoming {
    use {
        crate::{
            cluster_info::ClusterInfo,
            contact_info::Protocol::{QUIC, UDP},
            node::{MultihomingAddresses, Node},
        },
        solana_net_utils::multihomed_sockets::BindIpAddrs,
        std::{
            net::{IpAddr, UdpSocket},
            sync::Arc,
        },
    };

    #[derive(Debug, Clone)]
    pub struct NodeMultihoming {
        pub gossip_socket: Arc<[UdpSocket]>,
        pub addresses: MultihomingAddresses,
        pub bind_ip_addrs: Arc<BindIpAddrs>,
    }

    impl NodeMultihoming {
        /// Error handling note for `switch_active_interface(...)`
        ///
        /// Both self.gossip_socket and self.addresses are guaranteed to have the same length
        /// since they hold unique addresses and are bound by the length of self.bind_ip_addrs.
        ///
        /// `set_<protocol>_socket(...)` can only fail in 4 scenarios:
        /// 1. port is 0 (impossible - we can't bind to port 0)
        /// 2. ip is multicast (checked at startup)
        /// 3. ip is unspecified (checked at startup)
        /// 4. > 255 IPs (impossible - bounded by bind_ip_addrs.len())
        pub fn switch_active_interface(
            &self,
            interface: IpAddr,
            cluster_info: &ClusterInfo,
        ) -> Result<(), String> {
            if self.bind_ip_addrs.active() == interface {
                return Err(String::from("Specified interface already selected"));
            }
            // check the validity of the provided address
            let interface_index = self
                .bind_ip_addrs
                .iter()
                .position(|&e| e == interface)
                .ok_or_else(|| {
                    let addrs: &[IpAddr] = &self.bind_ip_addrs;
                    format!(
                        "Invalid interface address provided, registered interfaces are {addrs:?}",
                    )
                })?;

            // update gossip socket
            let gossip_addr = self.gossip_socket[interface_index]
                .local_addr()
                .map_err(|e| e.to_string())?;
            // Set the new gossip address in contact-info
            cluster_info
                .set_gossip_socket(gossip_addr)
                .map_err(|e| e.to_string())?;

            // update tvu ingress advertised socket
            let tvu_ingress_address = self.addresses.tvu[interface_index];
            cluster_info
                .set_tvu_socket(tvu_ingress_address)
                .map_err(|e| e.to_string())?;

            // tpu_quic
            let tpu_quic_address = self.addresses.tpu_quic[interface_index];
            cluster_info
                .set_tpu_quic(tpu_quic_address)
                .map_err(|e| e.to_string())?;

            // tpu_forwards_quic
            let tpu_forwards_quic_address = self.addresses.tpu_forwards_quic[interface_index];
            cluster_info
                .set_tpu_forwards_quic(tpu_forwards_quic_address)
                .map_err(|e| e.to_string())?;

            // tpu_vote_quic
            let tpu_vote_quic_address = self.addresses.tpu_vote_quic[interface_index];
            cluster_info
                .set_tpu_vote(QUIC, tpu_vote_quic_address)
                .map_err(|e| e.to_string())?;

            // tpu_vote (udp)
            let tpu_vote_address = self.addresses.tpu_vote[interface_index];
            cluster_info
                .set_tpu_vote(UDP, tpu_vote_address)
                .map_err(|e| e.to_string())?;

            // Update active index for tvu broadcast, tvu retransmit, and tpu forwarding client
            // This will never fail since we have checked index validity above
            let _new_ip_addr = self
                .bind_ip_addrs
                .set_active(interface_index)
                .expect("Interface index out of range");

            Ok(())
        }
    }

    impl From<&Node> for NodeMultihoming {
        fn from(node: &Node) -> Self {
            NodeMultihoming {
                gossip_socket: node.sockets.gossip.clone(),
                addresses: node.addresses.clone(),
                bind_ip_addrs: node.bind_ip_addrs.clone(),
            }
        }
    }
}

pub use multihoming::*;
