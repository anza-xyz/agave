use {
    agave_bls_sigverify::bls_sigverifier::NUM_SLOTS_FOR_VERIFY,
    agave_votor_messages::{migration::MigrationStatus, reward_certificate::NUM_SLOTS_FOR_REWARD},
    agave_votor_transport::{PeerList, PeerListSender},
    arc_swap::ArcSwap,
    crossbeam_channel::{RecvTimeoutError, Sender, bounded},
    solana_gossip::cluster_info::ClusterInfo,
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_runtime::bank_forks::SharableBanks,
    solana_signer::Signer,
    solana_tls_utils::NotifyKeyUpdate,
    std::{
        collections::{HashMap, HashSet},
        error::Error,
        net::SocketAddr,
        sync::Arc,
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

/// How often the endpoint peer_list is refreshed to track IP changes in gossip.
const PEER_LIST_REFRESH_INTERVAL: Duration = Duration::from_millis(500);

const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Builds and publishes the votor datagram endpoint's peer_list: the set of
/// staked validators (merged across an epoch boundary) paired with each peer's
/// votor server sockets from gossip. The transport endpoint uses it to filter
/// inbound connections, initiate outbound connections and to fan out packets.
pub struct PeerListUpdater {
    sharable_banks: SharableBanks,
    /// Publisher for the endpoint's peer_list snapshot.
    peer_list_sender: PeerListSender,
    /// Set of peers plugged into the peer_list regardless of stake.
    /// `None` resolves the address from gossip like any other peer, `Some` forces it,
    /// which local-cluster tests use to blackhole peers or point them at a spy listener.
    peer_overrides: Arc<ArcSwap<HashMap<Pubkey, Option<SocketAddr>>>>,
    /// Used to hold off connecting to peers until migration.
    migration_status: Arc<MigrationStatus>,
}

impl PeerListUpdater {
    #[cfg(feature = "dev-context-only-utils")]
    pub fn new_for_tests(
        sharable_banks: SharableBanks,
        peer_list_sender: PeerListSender,
        peer_overrides: Arc<ArcSwap<HashMap<Pubkey, Option<SocketAddr>>>>,
    ) -> Self {
        Self::new(
            sharable_banks,
            peer_list_sender,
            peer_overrides,
            Arc::new(MigrationStatus::post_migration_status()),
        )
    }

    pub fn new(
        sharable_banks: SharableBanks,
        peer_list_sender: PeerListSender,
        peer_overrides: Arc<ArcSwap<HashMap<Pubkey, Option<SocketAddr>>>>,
        migration_status: Arc<MigrationStatus>,
    ) -> Self {
        Self {
            sharable_banks,
            peer_list_sender,
            peer_overrides,
            migration_status,
        }
    }

    /// Publish the peer_list for the votor transport endpoint.
    ///
    /// The set encompasses every peer whose consensus input we may still want:
    /// - from `epoch(root_slot - NUM_SLOTS_FOR_REWARD)` and `epoch(root_slot)`. This
    ///   ensures we always admit all votes required to both vote and finalize the previous
    ///   epoch (including all possible vote credits).
    /// - from `epoch(root_slot) + 1` when within [`NUM_SLOTS_FOR_VERIFY`] of the epoch end.
    ///   `NUM_SLOTS_FOR_VERIFY` is the furthest ahead of root a vote/cert is accepted.
    ///
    /// Peer overrides join the set regardless of stake, which both admits their
    /// inbound connections and makes us push to them.
    ///
    /// Each peer's votor socket is resolved from gossip, unless an override pins it.
    /// Peers that have no address yet are added with `None`.
    ///
    /// Pushing is enabled only while this node is itself in the staked set: an
    /// unstaked node still admits the set so it can follow consensus, but staked
    /// peers would not admit it, so it must not dial out.
    pub fn refresh_peer_list(&self, cluster_info: &ClusterInfo, my_identity: &Pubkey) {
        let root = self.sharable_banks.root();
        let epoch_schedule = root.epoch_schedule();
        let (root_epoch, slot_index) = epoch_schedule.get_epoch_and_slot_index(root.slot());
        // Subtracting NUM_SLOTS_FOR_REWARD to make sure we are guaranteed to
        // catch all the votes that are coming late from nodes with high latency.
        let trailing_epoch =
            epoch_schedule.get_epoch(root.slot().saturating_sub(NUM_SLOTS_FOR_REWARD));

        // Only the pubkeys matter here; stake weights are unused.
        let mut peers: HashSet<Pubkey> = root
            .epoch_staked_nodes(root_epoch)
            .expect("Root bank retains epoch_stakes for its own epoch")
            .keys()
            .copied()
            .collect();
        // Admit the peers from the trailing_epoch (they can be different for the
        // first few slots after a boundary). Trailing epoch staked nodes may not be available
        // after warp, so we have to allow for that here.
        if root_epoch != trailing_epoch
            && let Some(nodes) = root.epoch_staked_nodes(trailing_epoch)
        {
            peers.extend(nodes.keys());
        }

        // Near the epoch end, admit the upcoming epoch's set so new voters can connect in advance.
        let slots_in_epoch = epoch_schedule.get_slots_in_epoch(root_epoch);
        let near_end = slot_index >= slots_in_epoch.saturating_sub(NUM_SLOTS_FOR_VERIFY);
        if near_end && let Some(next) = root.epoch_staked_nodes(root_epoch.saturating_add(1)) {
            peers.extend(next.keys());
        }

        let push_enabled =
            peers.contains(my_identity) && !self.migration_status.is_pre_feature_activation();
        let overrides = self.peer_overrides.load();
        let mut new_peers: HashMap<_, _> = cluster_info
            .query_contact_infos(peers.iter().chain(overrides.keys()), |node| {
                node.alpenglow()
            })
            .into_iter()
            .map(|(pubkey, socket)| (pubkey, socket.flatten()))
            .collect();
        // An override that pins an address wins over whatever gossip resolved to.
        for (pubkey, socket) in overrides.iter().filter(|(_, socket)| socket.is_some()) {
            new_peers.insert(*pubkey, *socket);
        }
        // Publish the latest version - this neither blocks nor fails.
        self.peer_list_sender.send_replace(Arc::new(PeerList {
            peers: new_peers,
            push_enabled,
        }));
    }
}

pub struct PeerListKeyUpdater {
    sender: Sender<Pubkey>,
}

impl NotifyKeyUpdate for PeerListKeyUpdater {
    fn update_key(&self, keypair: &Keypair) -> Result<(), Box<dyn Error>> {
        self.sender
            .send(keypair.pubkey())
            .map_err(|_| -> Box<dyn Error> { "PeerListService identity update rejected".into() })?;
        Ok(())
    }
}

/// Periodically publishes the votor transport endpoint's peer_list.
pub struct PeerListService {
    thread_hdl: JoinHandle<()>,
    key_updater: Arc<PeerListKeyUpdater>,
}

impl PeerListService {
    pub fn new(
        cluster_info: Arc<ClusterInfo>,
        peer_list: PeerListSender,
        sharable_banks: SharableBanks,
        peer_overrides: Arc<ArcSwap<HashMap<Pubkey, Option<SocketAddr>>>>,
        migration_status: Arc<MigrationStatus>,
    ) -> Self {
        let (key_updater_sender, key_updater_receiver) = bounded(16);
        let key_updater = Arc::new(PeerListKeyUpdater {
            sender: key_updater_sender,
        });
        let thread_hdl = Builder::new()
            .name("solVotorPeerUpd".to_string())
            .spawn(move || {
                let peer_list_updater = PeerListUpdater::new(
                    sharable_banks,
                    peer_list,
                    peer_overrides,
                    migration_status,
                );

                info!("AlpenglowPeerListService has started");
                let mut my_identity = cluster_info.id();
                // Populate the peer_list immediately at startup for admission control.
                peer_list_updater.refresh_peer_list(&cluster_info, &my_identity);
                let mut last_refresh = Instant::now();
                while !peer_list_updater.peer_list_sender.is_closed() {
                    match key_updater_receiver.recv_timeout(SHUTDOWN_POLL_INTERVAL) {
                        Ok(new_identity) => {
                            my_identity = new_identity;
                        }
                        Err(RecvTimeoutError::Disconnected) => break,
                        Err(RecvTimeoutError::Timeout) => {
                            if last_refresh.elapsed() < PEER_LIST_REFRESH_INTERVAL {
                                continue;
                            }
                        }
                    }
                    peer_list_updater.refresh_peer_list(&cluster_info, &my_identity);
                    last_refresh = Instant::now();
                }
                info!("AlpenglowPeerListService has stopped");
            })
            .unwrap();
        Self {
            thread_hdl,
            key_updater,
        }
    }

    /// Handle to register with the validator's key-update notifier so a
    /// `set-identity` refreshes the peer_list immediately.
    pub fn key_updater(&self) -> Arc<PeerListKeyUpdater> {
        self.key_updater.clone()
    }

    pub fn join(self) -> thread::Result<()> {
        self.thread_hdl.join()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        rand::Rng,
        solana_gossip::{
            cluster_info::ClusterInfo, contact_info::ContactInfo, crds::GossipRoute,
            crds_data::CrdsData, crds_value::CrdsValue, node::Node,
        },
        solana_keypair::Keypair,
        solana_net_utils::SocketAddrSpace,
        solana_pubkey::Pubkey,
        solana_runtime::{
            bank::Bank,
            bank_forks::BankForks,
            genesis_utils::{
                ValidatorVoteKeypairs, create_genesis_config_with_alpenglow_vote_accounts,
            },
        },
        solana_signer::Signer,
        solana_time_utils::timestamp,
        std::{
            collections::HashMap,
            net::{IpAddr, Ipv4Addr, SocketAddr},
            sync::{Arc, RwLock},
        },
        tokio::sync::watch,
    };

    fn update_cluster_info(
        cluster_info: &mut ClusterInfo,
        node_keypair_map: HashMap<Pubkey, Keypair>,
    ) {
        let node_contact_info = node_keypair_map
            .keys()
            .enumerate()
            .map(|(node_ix, pubkey)| {
                let mut contact_info = ContactInfo::new(*pubkey, 0, 0);

                assert!(
                    contact_info
                        .set_alpenglow((
                            Ipv4Addr::LOCALHOST,
                            8080_u16.saturating_add(node_ix as u16)
                        ))
                        .is_ok()
                );

                contact_info
            });

        for contact_info in node_contact_info {
            let node_pubkey = *contact_info.pubkey();

            let entry = CrdsValue::new(
                CrdsData::ContactInfo(contact_info),
                &node_keypair_map[&node_pubkey],
            );

            assert_eq!(node_pubkey, entry.label().pubkey());

            {
                let mut gossip_crds = cluster_info.gossip.crds.write();

                gossip_crds
                    .insert(entry, timestamp(), GossipRoute::LocalMessage)
                    .unwrap();
            }
        }
    }

    /// Create a number of nodes; each node will have exactly one vote account. Each vote account
    /// will have random stake in [1, 997), with the exception of the first few vote accounts
    /// having exactly 0 stake.
    fn create_bank_forks_and_cluster_info(
        num_nodes: usize,
        num_zero_stake_nodes: usize,
        base_slot: u64,
    ) -> (Arc<RwLock<BankForks>>, ClusterInfo, Vec<Pubkey>) {
        let mut rng = rand::rng();
        let validator_keypairs = (0..num_nodes)
            .map(|_| ValidatorVoteKeypairs::new(Keypair::new(), Keypair::new(), Keypair::new()))
            .collect::<Vec<ValidatorVoteKeypairs>>();

        let my_keypair = validator_keypairs
            .last()
            .unwrap()
            .node_keypair
            .insecure_clone();

        let node_keypair_map = validator_keypairs
            .iter()
            .map(|v| (v.node_keypair.pubkey(), v.node_keypair.insecure_clone()))
            .collect();
        let stakes = (0..num_nodes)
            .map(|node_ix| {
                if node_ix < num_zero_stake_nodes {
                    0
                } else {
                    rng.random_range(1..997)
                }
            })
            .collect();

        let genesis = create_genesis_config_with_alpenglow_vote_accounts(
            1_000_000_000,
            &validator_keypairs,
            stakes,
        );

        let mut bank0 = Bank::new_for_tests(&genesis.genesis_config);
        let base_slot_epoch = bank0.epoch_schedule().get_epoch(base_slot);
        bank0.set_epoch_stakes_for_test(base_slot_epoch, bank0.epoch_stakes(0).unwrap().clone());
        let bank_forks = BankForks::new_rw_arc(bank0);

        let mut cluster_info = ClusterInfo::new(
            Node::new_localhost_with_pubkey(&my_keypair.pubkey()).info,
            Arc::new(my_keypair),
            SocketAddrSpace::Unspecified,
        );
        update_cluster_info(&mut cluster_info, node_keypair_map);
        (
            bank_forks,
            cluster_info,
            validator_keypairs
                .iter()
                .map(|v| v.node_keypair.pubkey())
                .collect::<Vec<Pubkey>>(),
        )
    }

    /// A channel seeded the way `tvu` does it, before the first refresh lands.
    fn empty_peer_list_channel() -> (PeerListSender, watch::Receiver<Arc<PeerList>>) {
        watch::channel(Arc::new(PeerList {
            peers: HashMap::new(),
            push_enabled: false,
        }))
    }

    /// An unstaked node admits the whole staked set but keeps its client off.
    #[test]
    fn test_unstaked_node_admits_staked_set_without_pushing() {
        let slot_num = 12345000;
        let num_nodes = 10;
        // A fully-staked set, none of whose identities is the local node below.
        let (bank_forks, _, node_pubkeys) =
            create_bank_forks_and_cluster_info(num_nodes, 0, slot_num);

        // Identity that is not staked.
        let unstaked_kp = Keypair::new();
        let cluster_info = ClusterInfo::new(
            Node::new_localhost_with_pubkey(&unstaked_kp.pubkey()).info,
            Arc::new(unstaked_kp),
            SocketAddrSpace::Unspecified,
        );

        let (sender, receiver) = empty_peer_list_channel();
        let svc = PeerListUpdater::new_for_tests(
            bank_forks.read().unwrap().sharable_banks(),
            sender,
            Arc::default(),
        );

        svc.refresh_peer_list(&cluster_info, &cluster_info.id());

        let snapshot = receiver.borrow().clone();
        assert_eq!(
            snapshot.peers.keys().copied().collect::<HashSet<_>>(),
            node_pubkeys.iter().copied().collect::<HashSet<_>>(),
            "an unstaked node admits the entire staked set"
        );
        assert!(
            !snapshot.push_enabled,
            "an unstaked node must not push anything"
        );
    }

    #[test]
    fn test_refresh_honors_provided_identity() {
        let slot_num = 123456789;
        let num_nodes = 10;
        let num_zero_stake_nodes = 2;

        let (bank_forks, cluster_info, node_pubkeys) =
            create_bank_forks_and_cluster_info(num_nodes, num_zero_stake_nodes, slot_num);

        let (peerlist_sender, peerlist_receiver) = empty_peer_list_channel();
        let svc = PeerListUpdater::new_for_tests(
            bank_forks.read().unwrap().sharable_banks(),
            peerlist_sender,
            Arc::default(),
        );

        let unstaked = node_pubkeys[0];
        svc.refresh_peer_list(&cluster_info, &unstaked);
        assert!(
            !peerlist_receiver.borrow().push_enabled,
            "an unstaked identity must not push"
        );

        // After assuming a staked identity: pushing is enabled immediately
        let staked = node_pubkeys[num_zero_stake_nodes];
        svc.refresh_peer_list(&cluster_info, &staked);
        let snapshot = peerlist_receiver.borrow().clone();
        assert!(
            snapshot.push_enabled,
            "a staked identity joins the mesh right away"
        );
        assert_eq!(snapshot.peers.len(), num_nodes - num_zero_stake_nodes);
        assert!(
            snapshot.peers.values().all(|addr| addr.is_some()),
            "every staked peer resolves to its gossip socket"
        );
    }

    /// Overrides whose address should be resolved from gossip, as
    /// `--votor-peer-overrides` produces.
    fn gossip_resolved_overrides(
        peers: impl IntoIterator<Item = Pubkey>,
    ) -> Arc<ArcSwap<HashMap<Pubkey, Option<SocketAddr>>>> {
        Arc::new(ArcSwap::from_pointee(
            peers.into_iter().map(|peer| (peer, None)).collect(),
        ))
    }

    /// The RPC-node side: an unstaked node admits the staked set plus its
    /// overrides, and still does not push.
    #[test]
    fn test_overrides_admit_peers_on_unstaked_node() {
        let slot_num = 123456789;
        let num_nodes = 10;
        let num_zero_stake_nodes = 2;

        let (bank_forks, cluster_info, node_pubkeys) =
            create_bank_forks_and_cluster_info(num_nodes, num_zero_stake_nodes, slot_num);

        // A zero-stake peer, which only an override can admit.
        let overridden = node_pubkeys[1];
        let (peerlist_sender, peerlist_receiver) = empty_peer_list_channel();
        let svc = PeerListUpdater::new_for_tests(
            bank_forks.read().unwrap().sharable_banks(),
            peerlist_sender,
            gossip_resolved_overrides([overridden]),
        );

        let unstaked = node_pubkeys[0];
        svc.refresh_peer_list(&cluster_info, &unstaked);

        let snapshot = peerlist_receiver.borrow().clone();
        let expected: HashSet<_> = node_pubkeys[num_zero_stake_nodes..]
            .iter()
            .copied()
            .chain([overridden])
            .collect();
        assert_eq!(
            snapshot.peers.keys().copied().collect::<HashSet<_>>(),
            expected,
            "an unstaked node admits the staked set plus its overrides"
        );
        assert!(
            snapshot.peers.values().all(|addr| addr.is_some()),
            "every admitted peer resolves to its gossip socket"
        );
        assert!(
            !snapshot.push_enabled,
            "overrides do not make an unstaked node push"
        );
    }

    /// The validator side: a staked node pushes to overridden peers that hold no stake,
    /// on top of the full staked set.
    #[test]
    fn test_overrides_extend_staked_peer_list() {
        let slot_num = 123456789;
        let num_nodes = 10;
        let num_zero_stake_nodes = 2;

        let (bank_forks, cluster_info, node_pubkeys) =
            create_bank_forks_and_cluster_info(num_nodes, num_zero_stake_nodes, slot_num);

        // One zero-stake peer present in gossip, plus one staked peer that is already
        // in the merged set and must not be double counted.
        let unstaked_peer = node_pubkeys[0];
        let staked = node_pubkeys[num_zero_stake_nodes];
        let redundant_peer = node_pubkeys[num_zero_stake_nodes + 1];
        assert_ne!(
            redundant_peer,
            cluster_info.id(),
            "redundant_peer must not be self"
        );
        let (peerlist_sender, peerlist_receiver) = empty_peer_list_channel();
        let svc = PeerListUpdater::new_for_tests(
            bank_forks.read().unwrap().sharable_banks(),
            peerlist_sender,
            gossip_resolved_overrides([unstaked_peer, redundant_peer]),
        );

        svc.refresh_peer_list(&cluster_info, &staked);

        let snapshot = peerlist_receiver.borrow().clone();
        assert_eq!(
            snapshot.peers.len(),
            num_nodes - num_zero_stake_nodes + 1,
            "the staked set gains exactly the one unstaked override"
        );
        assert!(
            snapshot
                .peers
                .get(&unstaked_peer)
                .copied()
                .flatten()
                .is_some(),
            "an unstaked override is admitted and resolves to its gossip socket"
        );
        assert!(
            snapshot.push_enabled,
            "a staked node pushes to the peers it admits"
        );
    }

    #[test]
    fn test_override_missing_from_gossip_has_no_address() {
        let slot_num = 123456789;
        let (bank_forks, cluster_info, node_pubkeys) =
            create_bank_forks_and_cluster_info(10, 2, slot_num);

        let absent = Pubkey::new_unique();
        let (peerlist_sender, peerlist_receiver) = empty_peer_list_channel();
        let svc = PeerListUpdater::new_for_tests(
            bank_forks.read().unwrap().sharable_banks(),
            peerlist_sender,
            gossip_resolved_overrides([absent]),
        );

        svc.refresh_peer_list(&cluster_info, &node_pubkeys[0]);

        let snapshot = peerlist_receiver.borrow().clone();
        assert_eq!(
            snapshot.peers.get(&absent),
            Some(&None),
            "an override absent from gossip is desired but has no address yet"
        );
    }

    #[test]
    fn test_override_chosen_address_overrides_gossip() {
        let slot_num = 123456789;
        let num_zero_stake_nodes = 2;
        let (bank_forks, cluster_info, node_pubkeys) =
            create_bank_forks_and_cluster_info(10, num_zero_stake_nodes, slot_num);

        let staked_peer = node_pubkeys[num_zero_stake_nodes + 1];
        let pinned = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 65001);
        let (peerlist_sender, peerlist_receiver) = empty_peer_list_channel();
        let svc = PeerListUpdater::new_for_tests(
            bank_forks.read().unwrap().sharable_banks(),
            peerlist_sender,
            Arc::new(ArcSwap::from_pointee(HashMap::from([(
                staked_peer,
                Some(pinned),
            )]))),
        );

        svc.refresh_peer_list(&cluster_info, &node_pubkeys[num_zero_stake_nodes]);

        let snapshot = peerlist_receiver.borrow().clone();
        assert_eq!(
            snapshot.peers.get(&staked_peer),
            Some(&Some(pinned)),
            "a pinned address replaces the peer's gossip socket"
        );
    }
}
