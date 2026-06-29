#[cfg(feature = "dev-context-only-utils")]
use arc_swap::ArcSwap;
use {
    agave_votor_transport::PeerListSender,
    solana_clock::Epoch,
    solana_gossip::cluster_info::ClusterInfo,
    solana_pubkey::Pubkey,
    solana_runtime::bank_forks::SharableBanks,
    std::{
        collections::HashMap,
        net::{IpAddr, Ipv4Addr, SocketAddr},
        sync::Arc,
    },
};

/// Slots on either side of an epoch boundary during which the adjacent epoch's
/// staked set is merged into the peer_list, so peers transitioning across the
/// boundary are not transiently rejected.
const EPOCH_BOUNDARY_SLOTS: u64 = 100;

/// Builds and publishes the votor datagram endpoint's peer_list: the set of
/// staked validators (merged across an epoch boundary) paired with each peer's
/// votor server sockets from gossip. The transport endpoint uses it to filter
/// inbound connections, to initiate outbound connections and to fan out packets.
pub struct StakedValidatorsCache {
    sharable_banks: SharableBanks,

    /// Publisher for the endpoint's peer_list snapshot.
    peer_list: Option<PeerListSender>,

    /// Live, shared override of the (pubkey -> socket) set
    #[cfg(feature = "dev-context-only-utils")]
    test_overrides: Arc<ArcSwap<HashMap<Pubkey, SocketAddr>>>,
}

impl StakedValidatorsCache {
    pub fn new(
        sharable_banks: SharableBanks,
        peer_list: Option<PeerListSender>,
        #[cfg(feature = "dev-context-only-utils")] test_overrides: Arc<
            ArcSwap<HashMap<Pubkey, SocketAddr>>,
        >,
    ) -> Self {
        Self {
            sharable_banks,
            peer_list,
            #[cfg(feature = "dev-context-only-utils")]
            test_overrides,
        }
    }

    /// Lookup an epoch's staked node set. `None` if neither the root
    /// nor working bank knows this epoch.
    fn epoch_staked_nodes(&self, epoch: Epoch) -> Option<Arc<HashMap<Pubkey, u64>>> {
        let banks = [self.sharable_banks.root(), self.sharable_banks.working()];
        banks.iter().find_map(|bank| bank.epoch_staked_nodes(epoch))
    }

    /// Inject the test-only override pubkeys into `map` (dev builds only).
    /// Overrides are not epoch-specific, so this applies uniformly
    /// to whatever staked set was assembled. Entries carry a stake of 1,
    /// they exist only to exercise the network stack in tests.
    #[cfg_attr(
        not(feature = "dev-context-only-utils"),
        allow(unused_variables, clippy::unused_self)
    )]
    fn inject_overrides(&self, map: &mut HashMap<Pubkey, u64>) {
        #[cfg(feature = "dev-context-only-utils")]
        for (pk, _) in self.test_overrides.load().iter() {
            map.entry(*pk).or_insert(1);
        }
    }

    /// Resolve a peer's votor socket address, preferring a test override and
    /// falling back to its gossip contact info. `None` if neither knows it.
    fn resolve_socket(&self, pubkey: &Pubkey, cluster_info: &ClusterInfo) -> Option<SocketAddr> {
        #[cfg(feature = "dev-context-only-utils")]
        {
            if let Some(socket) = self.test_overrides.load().get(pubkey) {
                return Some(*socket);
            }
        }
        cluster_info
            .lookup_contact_info(pubkey, |node| node.alpenglow())
            .flatten()
    }

    /// Publish the peer_list for the votor transport endpoint.
    ///
    /// Near an epoch boundary (within [`EPOCH_BOUNDARY_SLOTS`] on either side)
    /// the adjacent epoch's staked set is merged in so peers transitioning
    /// across the boundary are not transiently rejected.
    ///
    /// Each peer's votor socket is resolved from gossip (or a test override).
    /// Peers that have no address yet are added with UNSPECIFIED address such
    /// that connections from them are admitted.
    pub fn refresh_peer_list(&self, cluster_info: &ClusterInfo) {
        let Some(peer_list) = self.peer_list.as_ref() else {
            return;
        };
        let working = self.sharable_banks.working();
        let epoch_schedule = working.epoch_schedule();
        let (epoch, slot_index) = epoch_schedule.get_epoch_and_slot_index(working.slot());
        let slots_in_epoch = epoch_schedule.get_slots_in_epoch(epoch);

        let near_start = slot_index < EPOCH_BOUNDARY_SLOTS;
        let near_end = slot_index >= slots_in_epoch.saturating_sub(EPOCH_BOUNDARY_SLOTS);

        let Some(base) = self.epoch_staked_nodes(epoch) else {
            error!(
                "StakedValidatorsCache can not update the peer list: epoch_staked_nodes not \
                 available for epoch {epoch}."
            );
            return;
        };
        let mut staked_nodes = (*base).clone();
        if near_start && epoch > 0 {
            if let Some(prev) = self.epoch_staked_nodes(epoch.saturating_sub(1)) {
                for (pubkey, stake) in prev.iter() {
                    staked_nodes.entry(*pubkey).or_insert(*stake);
                }
            }
        }
        if near_end {
            if let Some(next) = self.epoch_staked_nodes(epoch.saturating_add(1)) {
                for (pubkey, stake) in next.iter() {
                    staked_nodes.entry(*pubkey).or_insert(*stake);
                }
            }
        }
        self.inject_overrides(&mut staked_nodes);

        // Participate in votor only if this node is itself in the
        // staked set. An unstaked node publishes an empty peer_list, so the
        // transport neither connects to staked peers nor admits inbound connections.
        if !staked_nodes.contains_key(&cluster_info.id()) {
            if peer_list.send(Arc::new(HashMap::new())).is_err() {
                error!("Could not send empty peer list to the transport endpoint!");
            }
            return;
        }

        let snapshot = staked_nodes
            .into_keys()
            .map(|pubkey| {
                let addr = self
                    .resolve_socket(&pubkey, cluster_info)
                    // send an unspecified address as a sentinel
                    .unwrap_or(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0));
                (pubkey, addr)
            })
            .collect();
        // Publish the latest version via watch channel - this never blocks.
        if peer_list.send(Arc::new(snapshot)).is_err() {
            // This can only happen if the receivers are all dropped.
            error!("Could not send updated peer list to the transport endpoint!");
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        arc_swap::ArcSwap,
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
                let mut contact_info = ContactInfo::new(*pubkey, 0_u64, 0_u16);

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
                let mut gossip_crds = cluster_info.gossip.crds.write().unwrap();

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

        let node_keypair_map: HashMap<Pubkey, Keypair> = validator_keypairs
            .iter()
            .map(|v| (v.node_keypair.pubkey(), v.node_keypair.insecure_clone()))
            .collect();
        let stakes: Vec<u64> = (0..num_nodes)
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

    /// A refresh publishes one peer_list entry per staked node, each resolved to
    /// its gossip votor socket.
    #[test]
    fn test_refresh_peer_list_resolves_staked_sockets() {
        let slot_num = 325_000_000_u64;
        let num_nodes = 10_usize;
        let num_zero_stake_nodes = 2_usize;
        let (bank_forks, cluster_info, _) =
            create_bank_forks_and_cluster_info(num_nodes, num_zero_stake_nodes, slot_num);

        let (tx, rx) = watch::channel(Arc::new(HashMap::new()));
        let svc = StakedValidatorsCache::new(
            bank_forks.read().unwrap().sharable_banks(),
            Some(tx),
            Arc::new(ArcSwap::default()),
        );

        svc.refresh_peer_list(&cluster_info);

        let snapshot = rx.borrow();
        assert_eq!(snapshot.len(), num_nodes - num_zero_stake_nodes);
        // Every staked node resolved to a real gossip socket (no sentinel).
        assert!(snapshot.values().all(|addr| !addr.ip().is_unspecified()));
    }

    /// An unstaked node publishes an
    /// empty peer_list, so the transport is idle.
    #[test]
    fn test_unstaked_peer_list_remaims_empty() {
        let slot_num = 12345000u64;
        // A fully-staked set, none of whose identities is the local node below.
        let (bank_forks, _, _) = create_bank_forks_and_cluster_info(10, 0, slot_num);

        // Identity that is not staked.
        let unstaked_kp = Keypair::new();
        let cluster_info = ClusterInfo::new(
            Node::new_localhost_with_pubkey(&unstaked_kp.pubkey()).info,
            Arc::new(unstaked_kp),
            SocketAddrSpace::Unspecified,
        );

        let (tx, rx) = watch::channel(Arc::new(HashMap::from([(Pubkey::new_unique(), {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8000)
        })])));
        let svc = StakedValidatorsCache::new(
            bank_forks.read().unwrap().sharable_banks(),
            Some(tx),
            Arc::new(ArcSwap::default()),
        );

        svc.refresh_peer_list(&cluster_info);

        assert!(
            rx.borrow().is_empty(),
            "unstaked node must publish an empty peer_list"
        );
    }

    /// With no peer_list sender, a refresh is a no-op and must not panic.
    #[test]
    fn test_refresh_peer_list_without_sender_is_noop() {
        let slot_num = 325_000_000_u64;
        let (bank_forks, cluster_info, _) = create_bank_forks_and_cluster_info(4, 0, slot_num);
        let svc = StakedValidatorsCache::new(
            bank_forks.read().unwrap().sharable_banks(),
            None,
            Arc::new(ArcSwap::default()),
        );
        svc.refresh_peer_list(&cluster_info);
    }
}
