#[cfg(feature = "dev-context-only-utils")]
use arc_swap::ArcSwap;
use {
    agave_quic_datagram::allowlist::StakedNodesAllowlist,
    lazy_lru::LruCache,
    solana_clock::{Epoch, Slot},
    solana_gossip::cluster_info::ClusterInfo,
    solana_pubkey::Pubkey,
    solana_runtime::bank_forks::SharableBanks,
    std::{
        collections::HashMap,
        net::SocketAddr,
        sync::Arc,
        time::{Duration, Instant},
    },
};

/// Slots on either side of an epoch boundary during which the adjacent epoch's
/// staked set is merged into the inbound allowlist, so peers transitioning
/// across the boundary are not transiently rejected.
const EPOCH_BOUNDARY_SLOTS: u64 = 100;

struct StakedValidatorsCacheEntry {
    /// Identities and Votor endpoint addresses for all staked validators.
    peers: Vec<(Pubkey, SocketAddr)>,

    /// The time at which this entry was created
    creation_time: Instant,
}

/// Maintain `SocketAddr`s associated with all staked validators for a particular protocol (e.g.,
/// UDP, QUIC) over number of epochs.
///
/// We employ an LRU cache with a target size, mapping Epoch to cache entries that store the socket
/// information. We also track cache entry times, forcing recalculations of cache entries that are
/// accessed after a specified TTL.
pub struct StakedValidatorsCache {
    /// key: the epoch for which we have cached our stake validators list
    /// value: the cache entry
    cache: LruCache<Epoch, StakedValidatorsCacheEntry>,

    /// Time to live for cache entries
    ttl: Duration,

    /// Lock-free handle to the root/working banks.
    sharable_banks: SharableBanks,

    /// Whether to include the running validator's socket address in cache entries
    include_self: bool,

    /// Allowlist for the votor datagram endpoint.
    allowlist: Option<Arc<StakedNodesAllowlist>>,

    /// Live, shared override of the (pubkey -> socket) set
    #[cfg(feature = "dev-context-only-utils")]
    test_overrides: Arc<ArcSwap<HashMap<Pubkey, SocketAddr>>>,
}

impl StakedValidatorsCache {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        sharable_banks: SharableBanks,
        ttl: Duration,
        target_cache_size: usize,
        include_self: bool,
        allowlist: Option<Arc<StakedNodesAllowlist>>,
        #[cfg(feature = "dev-context-only-utils")] test_overrides: Arc<
            ArcSwap<HashMap<Pubkey, SocketAddr>>,
        >,
    ) -> Self {
        Self {
            cache: LruCache::new(target_cache_size),
            ttl,
            sharable_banks,
            include_self,
            allowlist,
            #[cfg(feature = "dev-context-only-utils")]
            test_overrides,
        }
    }

    /// Lookup an epoch's staked node set. `None` if neither the root
    /// nor working bank knows this epoch.
    fn epoch_staked_nodes_raw(&self, epoch: Epoch) -> Option<Arc<HashMap<Pubkey, u64>>> {
        let banks = [self.sharable_banks.root(), self.sharable_banks.working()];
        banks.iter().find_map(|bank| bank.epoch_staked_nodes(epoch))
    }

    /// Clone an epoch's staked node set, falling back to an empty set
    /// if the epoch is unknown.
    fn epoch_staked_nodes_cloned(&self, epoch: Epoch) -> HashMap<Pubkey, u64> {
        self.epoch_staked_nodes_raw(epoch)
            .map(|nodes| (*nodes).clone())
            .unwrap_or_else(|| {
                error!(
                    "StakedValidatorsCache: unknown Bank::epoch_staked_nodes for epoch: {epoch}"
                );
                HashMap::default()
            })
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

    /// Publish the inbound admission allowlist for QUIC endpoint.
    /// This is driven on the voting-service heartbeat so it stays fresh.
    ///
    /// Near an epoch boundary (within [`EPOCH_BOUNDARY_SLOTS`] on either side)
    /// the adjacent epoch's staked set is merged in so peers transitioning
    /// across the boundary are not transiently rejected.
    pub fn refresh_allowlist(&self) {
        let Some(allowlist) = self.allowlist.as_ref() else {
            return;
        };
        let working = self.sharable_banks.working();
        let epoch_schedule = working.epoch_schedule();
        let (epoch, slot_index) = epoch_schedule.get_epoch_and_slot_index(working.slot());
        let slots_in_epoch = epoch_schedule.get_slots_in_epoch(epoch);

        let near_start = slot_index < EPOCH_BOUNDARY_SLOTS;
        let near_end = slot_index >= slots_in_epoch.saturating_sub(EPOCH_BOUNDARY_SLOTS);

        let mut staked = self.epoch_staked_nodes_cloned(epoch);
        if near_start && epoch > 0 {
            if let Some(prev) = self.epoch_staked_nodes_raw(epoch.saturating_sub(1)) {
                for (pubkey, stake) in prev.iter() {
                    staked.entry(*pubkey).or_insert(*stake);
                }
            }
        }
        if near_end {
            if let Some(next) = self.epoch_staked_nodes_raw(epoch.saturating_add(1)) {
                for (pubkey, stake) in next.iter() {
                    staked.entry(*pubkey).or_insert(*stake);
                }
            }
        }
        self.inject_overrides(&mut staked);
        allowlist.swap(Arc::new(staked));
    }

    #[inline]
    fn cur_epoch(&self, slot: Slot) -> Epoch {
        self.sharable_banks
            .working()
            .epoch_schedule()
            .get_epoch(slot)
    }

    fn refresh_cache_entry(
        &mut self,
        epoch: Epoch,
        cluster_info: &ClusterInfo,
        update_time: Instant,
    ) {
        let mut epoch_staked_nodes = self.epoch_staked_nodes_cloned(epoch);
        self.inject_overrides(&mut epoch_staked_nodes);

        struct Node {
            pubkey: Pubkey,
            alpenglow_socket: SocketAddr,
        }

        let mut nodes: Vec<_> = epoch_staked_nodes
            .iter()
            .filter(|(pubkey, _stake)| self.include_self || pubkey != &&cluster_info.id())
            .filter_map(|(pubkey, _stake)| {
                // override sockets
                #[cfg(feature = "dev-context-only-utils")]
                {
                    if let Some(socket) = self.test_overrides.load().get(pubkey) {
                        return Some(Node {
                            pubkey: *pubkey,
                            alpenglow_socket: *socket,
                        });
                    }
                }
                cluster_info.lookup_contact_info(pubkey, |node| {
                    node.alpenglow().map(|alpenglow_socket| Node {
                        pubkey: *pubkey,
                        alpenglow_socket,
                    })
                })?
            })
            .collect();

        nodes.sort_unstable_by_key(|node| node.alpenglow_socket);
        nodes.dedup_by_key(|node| node.alpenglow_socket);

        let mut peers = Vec::with_capacity(nodes.len());
        for node in nodes {
            peers.push((node.pubkey, node.alpenglow_socket));
        }
        self.cache.put(
            epoch,
            StakedValidatorsCacheEntry {
                peers,
                creation_time: update_time,
            },
        );
    }

    pub fn get_staked_validators_by_slot(
        &mut self,
        slot: Slot,
        cluster_info: &ClusterInfo,
        access_time: Instant,
    ) -> (&[(Pubkey, SocketAddr)], bool) {
        let epoch = self.cur_epoch(slot);
        // For a given epoch, if we either:
        //
        // (1) have a cache entry that has expired
        // (2) have no existing cache entry
        //
        // then update the cache.
        let refresh_cache = self
            .cache
            .get(&epoch)
            .map(|v| Some(access_time) > v.creation_time.checked_add(self.ttl))
            .unwrap_or(true);

        if refresh_cache {
            self.refresh_cache_entry(epoch, cluster_info, access_time);
        }

        (
            // Unwrapping is fine here, since update_cache guarantees that we push a cache entry to
            // self.cache[epoch].
            self.cache.get(&epoch).map(|v| &*v.peers).unwrap(),
            refresh_cache,
        )
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::StakedValidatorsCache,
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
            net::Ipv4Addr,
            sync::{Arc, RwLock},
            time::{Duration, Instant},
        },
        test_case::test_case,
    };

    fn update_cluster_info(
        cluster_info: &mut ClusterInfo,
        node_keypair_map: HashMap<Pubkey, Keypair>,
    ) {
        // Update cluster info
        {
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

    #[test_case(1_usize, 0_usize)]
    #[test_case(10_usize, 2_usize)]
    #[test_case(50_usize, 7_usize)]
    fn test_detect_only_staked_nodes_and_refresh_after_ttl(
        num_nodes: usize,
        num_zero_stake_nodes: usize,
    ) {
        let slot_num = 325_000_000_u64;
        let (bank_forks, cluster_info, _) =
            create_bank_forks_and_cluster_info(num_nodes, num_zero_stake_nodes, slot_num);

        // Create our staked validators cache
        let mut svc = StakedValidatorsCache::new(
            bank_forks.read().unwrap().sharable_banks(),
            Duration::from_secs(5),
            5,
            true,
            None,
            Arc::new(ArcSwap::default()),
        );

        let now = Instant::now();

        let (sockets, refreshed) = svc.get_staked_validators_by_slot(slot_num, &cluster_info, now);

        assert!(refreshed);
        assert_eq!(
            num_nodes.saturating_sub(num_zero_stake_nodes),
            sockets.len()
        );
        assert_eq!(1, svc.len());

        // Re-fetch from the cache right before the 5-second deadline
        let (sockets, refreshed) = svc.get_staked_validators_by_slot(
            slot_num,
            &cluster_info,
            now.checked_add(Duration::from_secs_f64(4.999)).unwrap(),
        );

        assert!(!refreshed);
        assert_eq!(
            num_nodes.saturating_sub(num_zero_stake_nodes),
            sockets.len()
        );
        assert_eq!(1, svc.len());

        // Re-fetch from the cache right at the 5-second deadline - we still shouldn't refresh.
        let (sockets, refreshed) = svc.get_staked_validators_by_slot(
            slot_num,
            &cluster_info,
            now.checked_add(Duration::from_secs(5)).unwrap(),
        );
        assert!(!refreshed);
        assert_eq!(
            num_nodes.saturating_sub(num_zero_stake_nodes),
            sockets.len()
        );
        assert_eq!(1, svc.len());

        // Re-fetch from the cache right after the 5-second deadline - now we should refresh.
        let (sockets, refreshed) = svc.get_staked_validators_by_slot(
            slot_num,
            &cluster_info,
            now.checked_add(Duration::from_secs_f64(5.001)).unwrap(),
        );

        assert!(refreshed);
        assert_eq!(
            num_nodes.saturating_sub(num_zero_stake_nodes),
            sockets.len()
        );
        assert_eq!(1, svc.len());

        // Re-fetch from the cache well after the 5-second deadline - we should refresh.
        let (sockets, refreshed) = svc.get_staked_validators_by_slot(
            slot_num,
            &cluster_info,
            now.checked_add(Duration::from_secs(100)).unwrap(),
        );

        assert!(refreshed);
        assert_eq!(
            num_nodes.saturating_sub(num_zero_stake_nodes),
            sockets.len()
        );
        assert_eq!(1, svc.len());
    }

    #[test]
    fn test_cache_eviction() {
        let base_slot = 325_000_000_000;
        let (bank_forks, cluster_info, _) = create_bank_forks_and_cluster_info(50, 7, base_slot);

        // Create our staked validators cache
        let mut svc = StakedValidatorsCache::new(
            bank_forks.read().unwrap().sharable_banks(),
            Duration::from_secs(5),
            5,
            true,
            None,
            Arc::new(ArcSwap::default()),
        );

        assert_eq!(0, svc.len());
        assert!(svc.is_empty());

        let now = Instant::now();

        // Populate entries 1-9; the lazy LRU cache allows growth up to 2 * target_size = 10
        // before evicting, so all nine entries fit without any eviction.
        for entry_ix in 1_u64..=9_u64 {
            let (_, refreshed) = svc.get_staked_validators_by_slot(
                entry_ix.saturating_mul(base_slot),
                &cluster_info,
                now,
            );
            assert!(refreshed);
            assert_eq!(entry_ix as usize, svc.len());

            let (_, refreshed) = svc.get_staked_validators_by_slot(
                entry_ix.saturating_mul(base_slot),
                &cluster_info,
                now,
            );
            assert!(!refreshed);
            assert_eq!(entry_ix as usize, svc.len());
        }

        // Entry 10 triggers lazy eviction, reducing the cache to the target size of 5.
        let (_, refreshed) = svc.get_staked_validators_by_slot(10 * base_slot, &cluster_info, now);
        assert!(refreshed);
        assert_eq!(5, svc.len());

        // Epochs 1-5 should have been evicted (LRU).
        for entry_ix in 1_u64..=5_u64 {
            assert!(
                !svc.cache
                    .contains_key(&svc.cur_epoch(entry_ix.saturating_mul(base_slot)))
            );
        }

        // Epochs 6-10 should have entries.
        for entry_ix in 6_u64..=10_u64 {
            assert!(
                svc.cache
                    .contains_key(&svc.cur_epoch(entry_ix.saturating_mul(base_slot)))
            );
        }

        // Re-accessing entries 1-5 after TTL re-inserts them. With 5 already in cache (entries
        // 6-10), the cache again reaches 10 entries on the last insert, triggering another
        // eviction back to the target size of 5.
        for entry_ix in 1_u64..=5_u64 {
            let (_, refreshed) = svc.get_staked_validators_by_slot(
                entry_ix.saturating_mul(base_slot),
                &cluster_info,
                now.checked_add(Duration::from_secs(10)).unwrap(),
            );
            assert!(refreshed);
        }
        assert_eq!(5, svc.len());
    }

    #[test]
    fn test_only_update_once_per_epoch() {
        let slot_num = 325_000_000_u64;
        let num_nodes = 10_usize;
        let num_zero_stake_nodes = 2_usize;

        let (bank_forks, cluster_info, _) =
            create_bank_forks_and_cluster_info(num_nodes, num_zero_stake_nodes, slot_num);

        // Create our staked validators cache
        let mut svc = StakedValidatorsCache::new(
            bank_forks.read().unwrap().sharable_banks(),
            Duration::from_secs(5),
            5,
            true,
            None,
            Arc::new(ArcSwap::default()),
        );

        let now = Instant::now();

        let (_, refreshed) = svc.get_staked_validators_by_slot(slot_num, &cluster_info, now);
        assert!(refreshed);

        let (_, refreshed) = svc.get_staked_validators_by_slot(slot_num, &cluster_info, now);
        assert!(!refreshed);

        let (_, refreshed) = svc.get_staked_validators_by_slot(2 * slot_num, &cluster_info, now);
        assert!(refreshed);
    }

    #[test_case(1_usize)]
    #[test_case(10_usize)]
    fn test_exclude_self_from_cache(num_nodes: usize) {
        let slot_num = 325_000_000_u64;

        let (bank_forks, cluster_info, _) =
            create_bank_forks_and_cluster_info(num_nodes, 0, slot_num);

        let keypair = cluster_info.keypair().insecure_clone();

        let my_socket_addr = cluster_info
            .lookup_contact_info(&keypair.pubkey(), |node| node.alpenglow().unwrap())
            .unwrap();

        // Create our staked validators cache - set include_self to true
        let mut svc = StakedValidatorsCache::new(
            bank_forks.read().unwrap().sharable_banks(),
            Duration::from_secs(5),
            5,
            true,
            None,
            Arc::new(ArcSwap::default()),
        );

        let (peers, _) = svc.get_staked_validators_by_slot(slot_num, &cluster_info, Instant::now());
        assert_eq!(peers.len(), num_nodes);
        assert!(peers.iter().any(|(_, s)| s == &my_socket_addr));

        // Create our staked validators cache - set include_self to false
        let mut svc = StakedValidatorsCache::new(
            bank_forks.read().unwrap().sharable_banks(),
            Duration::from_secs(5),
            5,
            false,
            None,
            Arc::new(ArcSwap::default()),
        );

        let (peers, _) = svc.get_staked_validators_by_slot(slot_num, &cluster_info, Instant::now());
        // We should have num_nodes - 1 sockets, since we exclude our own socket address.
        assert_eq!(peers.len(), num_nodes.checked_sub(1).unwrap());
        assert!(!peers.iter().any(|(_, s)| s == &my_socket_addr));
    }
}
