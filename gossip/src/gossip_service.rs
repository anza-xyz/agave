//! The `gossip_service` module implements the network control plane.

use {
    crate::{
        cluster_info::{ClusterInfo, GOSSIP_CHANNEL_CAPACITY},
        cluster_info_metrics::submit_gossip_stats,
        contact_info::ContactInfo,
        epoch_specs::EpochSpecs,
    },
    crossbeam_channel::Sender,
    rand::{thread_rng, Rng},
    solana_client::{connection_cache::ConnectionCache, tpu_client::TpuClientWrapper},
    solana_keypair::Keypair,
    solana_net_utils::{SocketAddrSpace, DEFAULT_IP_ECHO_SERVER_THREADS},
    solana_perf::recycler::Recycler,
    solana_pubkey::Pubkey,
    solana_rpc_client::rpc_client::RpcClient,
    solana_runtime::bank_forks::BankForks,
    solana_signer::Signer,
    solana_streamer::{
        evicting_sender::EvictingSender,
        streamer::{self, StreamerReceiveStats},
    },
    solana_tpu_client::tpu_client::{TpuClient, TpuClientConfig},
    std::{
        collections::HashSet,
        net::{SocketAddr, TcpListener, UdpSocket},
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, RwLock,
        },
        thread::{self, sleep, Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

const SUBMIT_GOSSIP_STATS_INTERVAL: Duration = Duration::from_secs(2);

pub struct GossipService {
    thread_hdls: Vec<JoinHandle<()>>,
}

impl GossipService {
    pub fn new(
        cluster_info: &Arc<ClusterInfo>,
        bank_forks: Option<Arc<RwLock<BankForks>>>,
        gossip_sockets: Arc<[UdpSocket]>,
        gossip_validators: Option<HashSet<Pubkey>>,
        should_check_duplicate_instance: bool,
        stats_reporter_sender: Option<Sender<Box<dyn FnOnce() + Send>>>,
        exit: Arc<AtomicBool>,
    ) -> Self {
        let (request_sender, request_receiver) =
            EvictingSender::new_bounded(GOSSIP_CHANNEL_CAPACITY);
        trace!(
            "GossipService: id: {}, listening on primary interface: {:?}, all available \
             interfaces: {:?}",
            &cluster_info.id(),
            gossip_sockets[0].local_addr().unwrap(),
            gossip_sockets,
        );
        let socket_addr_space = *cluster_info.socket_addr_space();
        let gossip_receiver_stats = Arc::new(StreamerReceiveStats::new("gossip_receiver"));
        let t_receiver = streamer::receiver_atomic(
            "solRcvrGossip".to_string(),
            gossip_sockets.clone(),
            cluster_info.bind_ip_addrs(),
            exit.clone(),
            request_sender,
            Recycler::default(),
            gossip_receiver_stats.clone(),
            Some(Duration::from_millis(1)), // coalesce
            false,
            None,
            false,
        );
        let (consume_sender, listen_receiver) =
            EvictingSender::new_bounded(GOSSIP_CHANNEL_CAPACITY);
        let t_socket_consume = cluster_info.clone().start_socket_consume_thread(
            bank_forks.clone(),
            request_receiver,
            consume_sender,
            exit.clone(),
        );
        let (response_sender, response_receiver) =
            EvictingSender::new_bounded(GOSSIP_CHANNEL_CAPACITY);
        let t_listen = cluster_info.clone().listen(
            bank_forks.clone(),
            listen_receiver,
            response_sender.clone(),
            should_check_duplicate_instance,
            exit.clone(),
        );
        let t_gossip = cluster_info.clone().gossip(
            bank_forks.clone(),
            response_sender,
            gossip_validators,
            exit.clone(),
        );
        let t_responder = streamer::responder_atomic(
            "Gossip",
            gossip_sockets.clone(),
            cluster_info.bind_ip_addrs(),
            response_receiver,
            socket_addr_space,
            stats_reporter_sender,
        );
        let t_metrics = Builder::new()
            .name("solGossipMetr".to_string())
            .spawn({
                let cluster_info = cluster_info.clone();
                let mut epoch_specs = bank_forks.map(EpochSpecs::from);
                move || {
                    while !exit.load(Ordering::Relaxed) {
                        sleep(SUBMIT_GOSSIP_STATS_INTERVAL);
                        let stakes = epoch_specs
                            .as_mut()
                            .map(|epoch_specs| epoch_specs.current_epoch_staked_nodes())
                            .cloned()
                            .unwrap_or_default();

                        submit_gossip_stats(&cluster_info.stats, &cluster_info.gossip, &stakes);
                        gossip_receiver_stats.report();
                    }
                }
            })
            .unwrap();
        let thread_hdls = vec![
            t_receiver,
            t_responder,
            t_socket_consume,
            t_listen,
            t_gossip,
            t_metrics,
        ];
        Self { thread_hdls }
    }

    pub fn join(self) -> thread::Result<()> {
        for thread_hdl in self.thread_hdls {
            thread_hdl.join()?;
        }
        Ok(())
    }
}

pub fn discover_validators(
    entrypoint: &SocketAddr,
    num_nodes: usize,
    my_shred_version: u16,
    socket_addr_space: SocketAddrSpace,
) -> std::io::Result<Vec<ContactInfo>> {
    const DISCOVER_CLUSTER_TIMEOUT: Duration = Duration::from_secs(120);
    let (_all_peers, validators) = discover_peers(
        None,
        &vec![*entrypoint],
        Some(num_nodes),
        DISCOVER_CLUSTER_TIMEOUT,
        None,
        &[],
        None,
        my_shred_version,
        socket_addr_space,
    )?;
    Ok(validators)
}

pub fn discover_peers(
    keypair: Option<Keypair>,
    entrypoints: &Vec<SocketAddr>,
    num_nodes: Option<usize>, // num_nodes only counts validators, excludes spy nodes
    timeout: Duration,
    find_nodes_by_pubkey: Option<&[Pubkey]>,
    find_nodes_by_gossip_addr: &[SocketAddr],
    my_gossip_addr: Option<&SocketAddr>,
    my_shred_version: u16,
    socket_addr_space: SocketAddrSpace,
) -> std::io::Result<(
    Vec<ContactInfo>, // all gossip peers
    Vec<ContactInfo>, // tvu peers (validators)
)> {
    let keypair = keypair.unwrap_or_else(Keypair::new);
    let exit = Arc::new(AtomicBool::new(false));
    let (gossip_service, ip_echo, spy_ref) = make_node(
        keypair,
        entrypoints,
        exit.clone(),
        my_gossip_addr,
        my_shred_version,
        true, // should_check_duplicate_instance,
        socket_addr_space,
    );

    let id = spy_ref.id();
    info!("Entrypoints: {entrypoints:?}");
    info!("Node Id: {id:?}");
    if let Some(my_gossip_addr) = my_gossip_addr {
        info!("Gossip Address: {my_gossip_addr:?}");
    }

    let _ip_echo_server = ip_echo.map(|tcp_listener| {
        solana_net_utils::ip_echo_server(
            tcp_listener,
            DEFAULT_IP_ECHO_SERVER_THREADS,
            Some(my_shred_version),
        )
    });
    let (met_criteria, elapsed, all_peers, tvu_peers) = spy(
        spy_ref.clone(),
        num_nodes,
        timeout,
        find_nodes_by_pubkey,
        find_nodes_by_gossip_addr,
    );

    exit.store(true, Ordering::Relaxed);
    gossip_service.join().unwrap();

    if met_criteria {
        info!(
            "discover success in {}s...\n{}",
            elapsed.as_secs(),
            spy_ref.contact_info_trace()
        );
        return Ok((all_peers, tvu_peers));
    }

    if !tvu_peers.is_empty() {
        info!(
            "discover failed to match criteria by timeout...\n{}",
            spy_ref.contact_info_trace()
        );
        return Ok((all_peers, tvu_peers));
    }

    info!("discover failed...\n{}", spy_ref.contact_info_trace());
    Err(std::io::Error::other("Discover failed"))
}

/// Creates a TpuClient by selecting a valid node at random
pub fn get_client(
    nodes: &[ContactInfo],
    connection_cache: Arc<ConnectionCache>,
) -> TpuClientWrapper {
    let select = thread_rng().gen_range(0..nodes.len());

    let rpc_pubsub_url = format!("ws://{}/", nodes[select].rpc_pubsub().unwrap());
    let rpc_url = format!("http://{}", nodes[select].rpc().unwrap());

    match &*connection_cache {
        ConnectionCache::Quic(cache) => TpuClientWrapper::Quic(
            TpuClient::new_with_connection_cache(
                Arc::new(RpcClient::new(rpc_url)),
                rpc_pubsub_url.as_str(),
                TpuClientConfig::default(),
                cache.clone(),
            )
            .unwrap_or_else(|err| {
                panic!("Could not create TpuClient with Quic Cache {err:?}");
            }),
        ),
        ConnectionCache::Udp(cache) => TpuClientWrapper::Udp(
            TpuClient::new_with_connection_cache(
                Arc::new(RpcClient::new(rpc_url)),
                rpc_pubsub_url.as_str(),
                TpuClientConfig::default(),
                cache.clone(),
            )
            .unwrap_or_else(|err| {
                panic!("Could not create TpuClient with Udp Cache {err:?}");
            }),
        ),
    }
}

fn spy(
    spy_ref: Arc<ClusterInfo>,
    num_nodes: Option<usize>,
    timeout: Duration,
    find_nodes_by_pubkey: Option<&[Pubkey]>,
    find_nodes_by_gossip_addr: &[SocketAddr],
) -> (
    bool,             // if found the specified nodes
    Duration,         // elapsed time until found the nodes or timed-out
    Vec<ContactInfo>, // all gossip peers
    Vec<ContactInfo>, // tvu peers (validators)
) {
    let now = Instant::now();
    let mut met_criteria = false;
    let mut all_peers: Vec<ContactInfo> = Vec::new();
    let mut tvu_peers: Vec<ContactInfo> = Vec::new();
    let mut i = 1;
    while !met_criteria && now.elapsed() < timeout {
        all_peers = spy_ref
            .all_peers()
            .into_iter()
            .map(|x| x.0)
            .collect::<Vec<_>>();
        tvu_peers = spy_ref.tvu_peers(ContactInfo::clone);

        let found_nodes_by_pubkey = if let Some(pubkeys) = find_nodes_by_pubkey {
            pubkeys
                .iter()
                .all(|pubkey| all_peers.iter().any(|node| node.pubkey() == pubkey))
        } else {
            false
        };

        // if find_nodes_by_gossip_addr is not empty and all_peers contain
        // all the nodes from find_nodes_by_gossip_addr
        let found_nodes_by_gossip_addr = !find_nodes_by_gossip_addr.is_empty()
            && find_nodes_by_gossip_addr.iter().all(|node_addr| {
                all_peers
                    .iter()
                    .any(|peer| (*peer).gossip() == Some(*node_addr))
            });

        if let Some(num) = num_nodes {
            // Only consider validators and archives for `num_nodes`
            let mut nodes: Vec<ContactInfo> = tvu_peers.clone();
            nodes.sort_unstable_by_key(|node| *node.pubkey());
            nodes.dedup();

            if nodes.len() >= num {
                if found_nodes_by_pubkey || found_nodes_by_gossip_addr {
                    met_criteria = true;
                }

                if find_nodes_by_pubkey.is_none() && find_nodes_by_gossip_addr.is_empty() {
                    met_criteria = true;
                }
            }
        } else if found_nodes_by_pubkey || found_nodes_by_gossip_addr {
            met_criteria = true;
        }
        if i % 20 == 0 {
            info!("discovering...\n{}", spy_ref.contact_info_trace());
        }
        sleep(Duration::from_millis(
            crate::cluster_info::GOSSIP_SLEEP_MILLIS,
        ));
        i += 1;
    }
    (met_criteria, now.elapsed(), all_peers, tvu_peers)
}

pub fn make_node(
    keypair: Keypair,
    entrypoints: &[SocketAddr],
    exit: Arc<AtomicBool>,
    gossip_addr: Option<&SocketAddr>,
    shred_version: u16,
    should_check_duplicate_instance: bool,
    socket_addr_space: SocketAddrSpace,
) -> (GossipService, Option<TcpListener>, Arc<ClusterInfo>) {
    let (node, gossip_socket, ip_echo) = if let Some(gossip_addr) = gossip_addr {
        ClusterInfo::gossip_node(keypair.pubkey(), gossip_addr, shred_version)
    } else {
        ClusterInfo::spy_node(keypair.pubkey(), shred_version)
    };
    let cluster_info = ClusterInfo::new(node, Arc::new(keypair), socket_addr_space);

    cluster_info.set_entrypoints(
        entrypoints
            .iter()
            .map(ContactInfo::new_gossip_entry_point)
            .collect::<Vec<_>>(),
    );
    let gossip_sockets = Arc::new([gossip_socket]);
    let cluster_info = Arc::new(cluster_info);
    let gossip_service = GossipService::new(
        &cluster_info,
        None,
        gossip_sockets,
        None,
        should_check_duplicate_instance,
        None,
        exit,
    );
    (gossip_service, ip_echo, cluster_info)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{cluster_info::ClusterInfo, contact_info::ContactInfo, node::Node},
        std::sync::{atomic::AtomicBool, Arc},
    };

    #[test]
    // test that stage will exit when flag is set
    fn test_exit() {
        let exit = Arc::new(AtomicBool::new(false));
        let kp = Keypair::new();
        let tn = Node::new_localhost_with_pubkey(&kp.pubkey());
        let cluster_info =
            ClusterInfo::new(tn.info.clone(), Arc::new(kp), SocketAddrSpace::Unspecified);
        let c = Arc::new(cluster_info);
        let d = GossipService::new(
            &c,
            None,
            tn.sockets.gossip,
            None,
            true, // should_check_duplicate_instance
            None,
            exit.clone(),
        );
        exit.store(true, Ordering::Relaxed);
        d.join().unwrap();
    }

    #[test]
    fn test_gossip_services_spy() {
        const TIMEOUT: Duration = Duration::from_secs(5);
        let keypair = Keypair::new();
        let peer0 = solana_pubkey::new_rand();
        let peer1 = solana_pubkey::new_rand();
        let contact_info = ContactInfo::new_localhost(&keypair.pubkey(), 0);
        let peer0_info = ContactInfo::new_localhost(&peer0, 0);
        let peer1_info = ContactInfo::new_localhost(&peer1, 0);
        let cluster_info = ClusterInfo::new(
            contact_info,
            Arc::new(keypair),
            SocketAddrSpace::Unspecified,
        );
        cluster_info.insert_info(peer0_info.clone());
        cluster_info.insert_info(peer1_info);

        let spy_ref = Arc::new(cluster_info);

        let (met_criteria, elapsed, _, tvu_peers) = spy(spy_ref.clone(), None, TIMEOUT, None, &[]);
        assert!(!met_criteria);
        assert!((TIMEOUT..TIMEOUT + Duration::from_secs(1)).contains(&elapsed));
        assert_eq!(tvu_peers, spy_ref.tvu_peers(ContactInfo::clone));

        // Find num_nodes
        let (met_criteria, _, _, _) = spy(spy_ref.clone(), Some(1), TIMEOUT, None, &[]);
        assert!(met_criteria);
        let (met_criteria, _, _, _) = spy(spy_ref.clone(), Some(2), TIMEOUT, None, &[]);
        assert!(met_criteria);

        // Find specific node by pubkey
        let (met_criteria, _, _, _) = spy(spy_ref.clone(), None, TIMEOUT, Some(&[peer0]), &[]);
        assert!(met_criteria);
        let (met_criteria, _, _, _) = spy(
            spy_ref.clone(),
            None,
            TIMEOUT,
            Some(&[solana_pubkey::new_rand()]),
            &[],
        );
        assert!(!met_criteria);

        // Find num_nodes *and* specific node by pubkey
        let (met_criteria, _, _, _) = spy(spy_ref.clone(), Some(1), TIMEOUT, Some(&[peer0]), &[]);
        assert!(met_criteria);
        let (met_criteria, _, _, _) = spy(spy_ref.clone(), Some(3), TIMEOUT, Some(&[peer0]), &[]);
        assert!(!met_criteria);
        let (met_criteria, _, _, _) = spy(
            spy_ref.clone(),
            Some(1),
            TIMEOUT,
            Some(&[solana_pubkey::new_rand()]),
            &[],
        );
        assert!(!met_criteria);

        // Find specific node by gossip address
        let (met_criteria, _, _, _) = spy(
            spy_ref.clone(),
            None,
            TIMEOUT,
            None,
            &[peer0_info.gossip().unwrap()],
        );
        assert!(met_criteria);

        let (met_criteria, _, _, _) = spy(
            spy_ref,
            None,
            TIMEOUT,
            None,
            &["1.1.1.1:1234".parse().unwrap()],
        );
        assert!(!met_criteria);
    }
}
