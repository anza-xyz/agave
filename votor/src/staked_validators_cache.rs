#![allow(dead_code)]

use {
    crate::voting_service::AlpenglowPortOverride,
    lru::LruCache,
    solana_clock::{Epoch, Slot},
    solana_epoch_schedule::EpochSchedule,
    solana_gossip::cluster_info::ClusterInfo,
    solana_pubkey::Pubkey,
    solana_runtime::bank_forks::BankForks,
    std::{
        collections::HashMap,
        net::SocketAddr,
        sync::{Arc, RwLock},
        time::{Duration, Instant},
    },
};

struct StakedValidatorsCacheEntry {
    /// Alpenglow Sockets associated with the staked validators
    alpenglow_sockets: Vec<SocketAddr>,

    /// The time at which this entry was created
    creation_time: Instant,
}

/// Maintain `SocketAddr`s associated with all staked validators for a particular protocol (e.g.,
/// UDP, QUIC) over number of epochs.
///
/// We employ an LRU cache with capped size, mapping Epoch to cache entries that store the socket
/// information. We also track cache entry times, forcing recalculations of cache entries that are
/// accessed after a specified TTL.
pub struct StakedValidatorsCache {
    /// key: the epoch for which we have cached our stake validators list
    /// value: the cache entry
    cache: LruCache<Epoch, StakedValidatorsCacheEntry>,

    /// Time to live for cache entries
    ttl: Duration,

    /// Bank forks
    bank_forks: Arc<RwLock<BankForks>>,

    // Cache Epoch schedule since it never changes
    epoch_schedule: EpochSchedule,

    /// Whether to include the running validator's socket address in cache entries
    include_self: bool,

    /// Optional override for Alpenglow port, used for testing purposes
    alpenglow_port_override: Option<AlpenglowPortOverride>,

    /// timestamp of the last alpenglow port override we read
    alpenglow_port_override_last_modified: Instant,
}

impl StakedValidatorsCache {
    pub fn new(
        bank_forks: Arc<RwLock<BankForks>>,
        ttl: Duration,
        max_cache_size: usize,
        include_self: bool,
        alpenglow_port_override: Option<AlpenglowPortOverride>,
    ) -> Self {
        let epoch_schedule = bank_forks
            .read()
            .unwrap()
            .working_bank()
            .epoch_schedule()
            .clone();
        Self {
            cache: LruCache::new(max_cache_size),
            ttl,
            bank_forks,
            epoch_schedule,
            include_self,
            alpenglow_port_override,
            alpenglow_port_override_last_modified: Instant::now(),
        }
    }

    #[inline]
    fn cur_epoch(&self, slot: Slot) -> Epoch {
        self.epoch_schedule.get_epoch(slot)
    }

    fn refresh_cache_entry(
        &mut self,
        epoch: Epoch,
        cluster_info: &ClusterInfo,
        update_time: Instant,
    ) {
        let banks = {
            let bank_forks = self.bank_forks.read().unwrap();
            [bank_forks.root_bank(), bank_forks.working_bank()]
        };

        let epoch_staked_nodes = banks
            .iter()
            .find_map(|bank| bank.epoch_staked_nodes(epoch))
            .unwrap_or_else(|| {
                error!(
                    "StakedValidatorsCache::get: unknown Bank::epoch_staked_nodes for epoch: \
                     {epoch}"
                );
                Arc::<HashMap<Pubkey, u64>>::default()
            });

        struct Node {
            pubkey: Pubkey,
            stake: u64,
            alpenglow_socket: SocketAddr,
        }

        let mut nodes: Vec<_> = epoch_staked_nodes
            .iter()
            .filter(|(pubkey, stake)| {
                let positive_stake = **stake > 0;
                let not_self = pubkey != &&cluster_info.id();

                positive_stake && (self.include_self || not_self)
            })
            .filter_map(|(pubkey, stake)| {
                cluster_info.lookup_contact_info(pubkey, |node| {
                    node.alpenglow().map(|alpenglow_socket| Node {
                        pubkey: *pubkey,
                        stake: *stake,
                        alpenglow_socket,
                    })
                })?
            })
            .collect();

        nodes.dedup_by_key(|node| node.alpenglow_socket);
        nodes.sort_unstable_by(|a, b| a.stake.cmp(&b.stake));

        let mut alpenglow_sockets = Vec::with_capacity(nodes.len());
        let override_map = self
            .alpenglow_port_override
            .as_ref()
            .map(|x| x.get_override_map());
        for node in nodes {
            let alpenglow_socket = node.alpenglow_socket;
            let socket = if let Some(override_map) = &override_map {
                // If we have an override, use it.
                override_map
                    .get(&node.pubkey)
                    .cloned()
                    .unwrap_or(alpenglow_socket)
            } else {
                alpenglow_socket
            };
            alpenglow_sockets.push(socket);
        }
        self.cache.push(
            epoch,
            StakedValidatorsCacheEntry {
                alpenglow_sockets,
                creation_time: update_time,
            },
        );
    }

    pub fn get_staked_validators_by_slot(
        &mut self,
        slot: Slot,
        cluster_info: &ClusterInfo,
        access_time: Instant,
    ) -> (&[SocketAddr], bool) {
        let epoch = self.cur_epoch(slot);
        // Check if self.alpenglow_port_override has a different last_modified.
        // Immediately refresh the cache if it does.
        if let Some(alpenglow_port_override) = &self.alpenglow_port_override {
            if alpenglow_port_override.has_new_override(self.alpenglow_port_override_last_modified)
            {
                self.alpenglow_port_override_last_modified =
                    alpenglow_port_override.last_modified();
                trace!(
                    "refreshing cache entry for epoch {epoch} due to alpenglow port override \
                     last_modified change"
                );
                self.refresh_cache_entry(epoch, cluster_info, access_time);
            }
        }

        self.get_staked_validators_by_epoch(epoch, cluster_info, access_time)
    }

    fn get_staked_validators_by_epoch(
        &mut self,
        epoch: Epoch,
        cluster_info: &ClusterInfo,
        access_time: Instant,
    ) -> (&[SocketAddr], bool) {
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
            self.cache
                .get(&epoch)
                .map(|v| &*v.alpenglow_sockets)
                .unwrap(),
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
        crate::voting_service::AlpenglowPortOverride,
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
                create_genesis_config_with_alpenglow_vote_accounts, ValidatorVoteKeypairs,
            },
        },
        solana_signer::Signer,
        solana_time_utils::timestamp,
        std::{
            collections::HashMap,
            net::{Ipv4Addr, SocketAddr},
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

                    contact_info
                        .set_alpenglow((
                            Ipv4Addr::LOCALHOST,
                            8080_u16.saturating_add(node_ix as u16),
                        ))
                        .unwrap();

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
        let mut rng = rand::thread_rng();
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
                    rng.gen_range(1..997)
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
        let mut svc = StakedValidatorsCache::new(bank_forks, Duration::from_secs(5), 5, true, None);

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
        let mut svc = StakedValidatorsCache::new(bank_forks, Duration::from_secs(5), 5, true, None);

        assert_eq!(0, svc.len());
        assert!(svc.is_empty());

        let now = Instant::now();

        // Populate the first five entries; accessing the cache once again shouldn't trigger any
        // refreshes.
        for entry_ix in 1_u64..=5_u64 {
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

        // Entry 6 - this shouldn't increase the cache length.
        let (_, refreshed) = svc.get_staked_validators_by_slot(6 * base_slot, &cluster_info, now);
        assert!(refreshed);
        assert_eq!(5, svc.len());

        // Epoch 1 should have been evicted
        assert!(!svc.cache.contains(&svc.cur_epoch(base_slot)));

        // Epochs 2 - 6 should have entries
        for entry_ix in 2_u64..=6_u64 {
            assert!(svc
                .cache
                .contains(&svc.cur_epoch(entry_ix.saturating_mul(base_slot))));
        }

        // Accessing the cache after TTL should recalculate everything; the size remains 5, since
        // we only ever lazily evict cache entries.
        for entry_ix in 1_u64..=5_u64 {
            let (_, refreshed) = svc.get_staked_validators_by_slot(
                entry_ix.saturating_mul(base_slot),
                &cluster_info,
                now.checked_add(Duration::from_secs(10)).unwrap(),
            );
            assert!(refreshed);
            assert_eq!(5, svc.len());
        }
    }

    #[test]
    fn test_only_update_once_per_epoch() {
        let slot_num = 325_000_000_u64;
        let num_nodes = 10_usize;
        let num_zero_stake_nodes = 2_usize;

        let (bank_forks, cluster_info, _) =
            create_bank_forks_and_cluster_info(num_nodes, num_zero_stake_nodes, slot_num);

        // Create our staked validators cache
        let mut svc = StakedValidatorsCache::new(bank_forks, Duration::from_secs(5), 5, true, None);

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
        let mut svc =
            StakedValidatorsCache::new(bank_forks.clone(), Duration::from_secs(5), 5, true, None);

        let (sockets, _) =
            svc.get_staked_validators_by_slot(slot_num, &cluster_info, Instant::now());
        assert_eq!(sockets.len(), num_nodes);
        assert!(sockets.contains(&my_socket_addr));

        // Create our staked validators cache - set include_self to false
        let mut svc =
            StakedValidatorsCache::new(bank_forks.clone(), Duration::from_secs(5), 5, false, None);

        let (sockets, _) =
            svc.get_staked_validators_by_slot(slot_num, &cluster_info, Instant::now());
        // We should have num_nodes - 1 sockets, since we exclude our own socket address.
        assert_eq!(sockets.len(), num_nodes.checked_sub(1).unwrap());
        assert!(!sockets.contains(&my_socket_addr));
    }

    #[test]
    fn test_alpenglow_port_override() {
        agave_logger::setup();
        let (bank_forks, cluster_info, node_pubkeys) = create_bank_forks_and_cluster_info(3, 0, 1);
        let pubkey_b = node_pubkeys[1];

        let alpenglow_port_override = AlpenglowPortOverride::default();
        let blackhole_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();

        // Create our staked validators cache - set include_self to false
        let mut svc = StakedValidatorsCache::new(
            bank_forks.clone(),
            Duration::from_secs(5),
            5,
            false,
            Some(alpenglow_port_override.clone()),
        );
        // Nothing in the override, so we should get the original socket addresses.
        let (sockets, _) = svc.get_staked_validators_by_slot(0, &cluster_info, Instant::now());
        assert_eq!(sockets.len(), 2);
        assert!(!sockets.contains(&blackhole_addr));

        // Add an override for pubkey_B, and check that we get the overridden socket address.
        alpenglow_port_override.update_override(HashMap::from([(pubkey_b, blackhole_addr)]));
        let (sockets, _) = svc.get_staked_validators_by_slot(0, &cluster_info, Instant::now());
        assert_eq!(sockets.len(), 2);
        // Sort sockets to ensure the blackhole address is at index 0.
        let mut sockets: Vec<_> = sockets.to_vec();
        sockets.sort();
        assert_eq!(sockets[0], blackhole_addr);
        assert_ne!(sockets[1], blackhole_addr);

        // Now clear the override, and check that we get the original socket addresses.
        alpenglow_port_override.clear();
        let (sockets, _) = svc.get_staked_validators_by_slot(0, &cluster_info, Instant::now());
        assert_eq!(sockets.len(), 2);
        assert!(!sockets.contains(&blackhole_addr));
    }
}
