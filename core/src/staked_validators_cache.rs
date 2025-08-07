use {
    crate::voting_service::AlpenglowPortOverride,
    lru::LruCache,
    solana_clock::{Epoch, Slot},
    solana_gossip::{cluster_info::ClusterInfo, contact_info::Protocol},
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
    /// TPU Vote Sockets associated with the staked validators
    validator_sockets: Vec<SocketAddr>,

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

    /// Protocol
    protocol: Protocol,

    /// Whether to include the running validator's socket address in cache entries
    include_self: bool,

    /// Optional override for Alpenglow port, used for testing purposes
    alpenglow_port_override: Option<AlpenglowPortOverride>,

    /// timestamp of the last alpenglow port override we read
    alpenglow_port_override_last_modified: Instant,
}

enum PortsToUse {
    TpuVote,
    Alpenglow,
}

impl StakedValidatorsCache {
    pub fn new(
        bank_forks: Arc<RwLock<BankForks>>,
        protocol: Protocol,
        ttl: Duration,
        max_cache_size: usize,
        include_self: bool,
        alpenglow_port_override: Option<AlpenglowPortOverride>,
    ) -> Self {
        Self {
            cache: LruCache::new(max_cache_size),
            ttl,
            bank_forks,
            protocol,
            include_self,
            alpenglow_port_override,
            alpenglow_port_override_last_modified: Instant::now(),
        }
    }

    #[inline]
    fn cur_epoch(&self, slot: Slot) -> Epoch {
        self.bank_forks
            .read()
            .unwrap()
            .working_bank()
            .epoch_schedule()
            .get_epoch(slot)
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

        let epoch_staked_nodes = banks.iter().find_map(|bank| bank.epoch_staked_nodes(epoch)).unwrap_or_else(|| {
            error!("StakedValidatorsCache::get: unknown Bank::epoch_staked_nodes for epoch: {epoch}");
            Arc::<HashMap<Pubkey, u64>>::default()
        });

        struct Node {
            pubkey: Pubkey,
            stake: u64,
            tpu_socket: SocketAddr,
            // TODO(wen): this should not be an Option after BLS all-to-all is submitted.
            alpenglow_socket: Option<SocketAddr>,
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
                    let tpu_socket = node.tpu_vote(self.protocol);
                    let alpenglow_socket = node.alpenglow();
                    // To not change current behavior, we only consider nodes that have a
                    // TPU socket, and ignore nodes that only have an Alpenglow socket.
                    // TODO(wen): tpu_socket is no longer needed after Alpenglow migration.
                    tpu_socket.map(|tpu_socket| Node {
                        pubkey: *pubkey,
                        stake: *stake,
                        tpu_socket,
                        alpenglow_socket,
                    })
                })?
            })
            .collect();

        // TODO(wen): After Alpenglow vote is no longer transaction, we dedup by alpenglow socket.
        nodes.dedup_by_key(|node| node.tpu_socket);
        nodes.sort_unstable_by(|a, b| a.stake.cmp(&b.stake));

        let mut validator_sockets = Vec::new();
        let mut alpenglow_sockets = Vec::new();
        let override_map = self
            .alpenglow_port_override
            .as_ref()
            .map(|x| x.get_override_map());
        for node in nodes {
            validator_sockets.push(node.tpu_socket);

            if let Some(alpenglow_socket) = node.alpenglow_socket {
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
        }
        self.cache.push(
            epoch,
            StakedValidatorsCacheEntry {
                validator_sockets,
                alpenglow_sockets,
                creation_time: update_time,
            },
        );
    }

    pub fn get_staked_validators_by_slot_with_tpu_vote_ports(
        &mut self,
        slot: Slot,
        cluster_info: &ClusterInfo,
        access_time: Instant,
    ) -> (&[SocketAddr], bool) {
        self.get_staked_validators_by_epoch(
            self.cur_epoch(slot),
            cluster_info,
            access_time,
            PortsToUse::TpuVote,
        )
    }

    pub fn get_staked_validators_by_slot_with_alpenglow_ports(
        &mut self,
        slot: Slot,
        cluster_info: &ClusterInfo,
        access_time: Instant,
    ) -> (&[SocketAddr], bool) {
        // Check if self.alpenglow_port_override has a different last_modified.
        // Immediately refresh the cache if it does.
        if let Some(alpenglow_port_override) = &self.alpenglow_port_override {
            if alpenglow_port_override.has_new_override(self.alpenglow_port_override_last_modified)
            {
                self.alpenglow_port_override_last_modified =
                    alpenglow_port_override.last_modified();
                trace!(
                        "refreshing cache entry for epoch {} due to alpenglow port override last_modified change",
                        self.cur_epoch(slot)
                    );
                self.refresh_cache_entry(self.cur_epoch(slot), cluster_info, access_time);
            }
        }

        self.get_staked_validators_by_epoch(
            self.cur_epoch(slot),
            cluster_info,
            access_time,
            PortsToUse::Alpenglow,
        )
    }

    fn get_staked_validators_by_epoch(
        &mut self,
        epoch: Epoch,
        cluster_info: &ClusterInfo,
        access_time: Instant,
        ports_to_use: PortsToUse,
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
            .map(|v| access_time > v.creation_time + self.ttl)
            .unwrap_or(true);

        if refresh_cache {
            self.refresh_cache_entry(epoch, cluster_info, access_time);
        }

        (
            // Unwrapping is fine here, since update_cache guarantees that we push a cache entry to
            // self.cache[epoch].
            self.cache
                .get(&epoch)
                .map(|v| match ports_to_use {
                    PortsToUse::TpuVote => &*v.validator_sockets,
                    PortsToUse::Alpenglow => &*v.alpenglow_sockets,
                })
                .unwrap(),
            refresh_cache,
        )
    }

    pub fn len(&self) -> usize {
        self.cache.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::StakedValidatorsCache,
        crate::voting_service::AlpenglowPortOverride,
        solana_account::AccountSharedData,
        solana_clock::{Clock, Slot},
        solana_genesis_config::GenesisConfig,
        solana_gossip::{
            cluster_info::{ClusterInfo, Node},
            contact_info::{ContactInfo, Protocol},
            crds::GossipRoute,
            crds_data::CrdsData,
            crds_value::CrdsValue,
        },
        solana_keypair::Keypair,
        solana_ledger::genesis_utils::{create_genesis_config, GenesisConfigInfo},
        solana_pubkey::Pubkey,
        solana_runtime::{bank::Bank, bank_forks::BankForks, epoch_stakes::VersionedEpochStakes},
        solana_signer::Signer,
        solana_streamer::socket::SocketAddrSpace,
        solana_time_utils::timestamp,
        solana_vote::vote_account::{VoteAccount, VoteAccountsHashMap},
        solana_vote_program::vote_state::{VoteInit, VoteState, VoteStateVersions},
        std::{
            collections::HashMap,
            iter::{repeat, repeat_with},
            net::{Ipv4Addr, SocketAddr},
            sync::{Arc, RwLock},
            time::{Duration, Instant},
        },
        test_case::{test_case, test_matrix},
    };

    fn new_rand_vote_account<R: rand::Rng>(
        rng: &mut R,
        node_pubkey: Option<Pubkey>,
    ) -> (AccountSharedData, VoteState) {
        let vote_init = VoteInit {
            node_pubkey: node_pubkey.unwrap_or_else(Pubkey::new_unique),
            authorized_voter: Pubkey::new_unique(),
            authorized_withdrawer: Pubkey::new_unique(),
            commission: rng.gen(),
        };
        let clock = Clock {
            slot: rng.gen(),
            epoch_start_timestamp: rng.gen(),
            epoch: rng.gen(),
            leader_schedule_epoch: rng.gen(),
            unix_timestamp: rng.gen(),
        };
        let vote_state = VoteState::new(&vote_init, &clock);
        let account = AccountSharedData::new_data(
            rng.gen(), // lamports
            &VoteStateVersions::new_current(vote_state.clone()),
            &solana_sdk_ids::vote::id(), // owner
        )
        .unwrap();
        (account, vote_state)
    }

    fn new_rand_vote_accounts<R: rand::Rng>(
        rng: &mut R,
        num_nodes: usize,
        num_zero_stake_nodes: usize,
    ) -> impl Iterator<Item = (Keypair, Keypair, /*stake:*/ u64, VoteAccount)> + '_ {
        let node_keypairs: Vec<_> = repeat_with(Keypair::new).take(num_nodes).collect();

        repeat(0..num_nodes).flatten().map(move |node_ix| {
            let node_keypair = node_keypairs[node_ix].insecure_clone();
            let vote_account_keypair = Keypair::new();

            let (account, _) = new_rand_vote_account(rng, Some(node_keypair.pubkey()));
            let stake = if node_ix < num_zero_stake_nodes {
                0
            } else {
                rng.gen_range(1..997)
            };
            let vote_account = VoteAccount::try_from(account).unwrap();
            (vote_account_keypair, node_keypair, stake, vote_account)
        })
    }

    struct StakedValidatorsCacheHarness {
        bank: Bank,
        cluster_info: ClusterInfo,
    }

    impl StakedValidatorsCacheHarness {
        pub fn new(genesis_config: &GenesisConfig, keypair: Keypair) -> Self {
            let bank = Bank::new_for_tests(genesis_config);

            let cluster_info = ClusterInfo::new(
                Node::new_localhost_with_pubkey(&keypair.pubkey()).info,
                Arc::new(keypair),
                SocketAddrSpace::Unspecified,
            );

            Self { bank, cluster_info }
        }

        pub fn with_vote_accounts(
            mut self,
            slot: Slot,
            node_keypair_map: HashMap<Pubkey, Keypair>,
            vote_accounts: VoteAccountsHashMap,
            protocol: Protocol,
        ) -> Self {
            // Update cluster info
            {
                let node_contact_info =
                    node_keypair_map
                        .keys()
                        .enumerate()
                        .map(|(node_ix, pubkey)| {
                            let mut contact_info = ContactInfo::new(*pubkey, 0_u64, 0_u16);

                            assert!(contact_info
                                .set_tpu_vote(
                                    protocol,
                                    (Ipv4Addr::LOCALHOST, 8005 + node_ix as u16),
                                )
                                .is_ok());

                            assert!(contact_info
                                .set_alpenglow((Ipv4Addr::LOCALHOST, 8080 + node_ix as u16))
                                .is_ok());

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
                        let mut gossip_crds = self.cluster_info.gossip.crds.write().unwrap();

                        gossip_crds
                            .insert(entry, timestamp(), GossipRoute::LocalMessage)
                            .unwrap();
                    }
                }
            }

            // Update bank
            let epoch_num = self.bank.epoch_schedule().get_epoch(slot);
            let epoch_stakes = VersionedEpochStakes::new_for_tests(vote_accounts, epoch_num);

            self.bank.set_epoch_stakes_for_test(epoch_num, epoch_stakes);

            self
        }

        pub fn bank_forks(self) -> (Arc<RwLock<BankForks>>, ClusterInfo) {
            let bank_forks = self.bank.wrap_with_bank_forks_for_tests().1;
            (bank_forks, self.cluster_info)
        }
    }

    /// Create a number of nodes; each node will have one or more vote accounts. Each vote account
    /// will have random stake in [1, 997), with the exception of the first few vote accounts
    /// having exactly 0 stake.
    fn build_epoch_stakes(
        num_nodes: usize,
        num_zero_stake_vote_accounts: usize,
        num_vote_accounts: usize,
    ) -> (HashMap<Pubkey, Keypair>, VoteAccountsHashMap) {
        let mut rng = rand::thread_rng();

        let vote_accounts: Vec<_> =
            new_rand_vote_accounts(&mut rng, num_nodes, num_zero_stake_vote_accounts)
                .take(num_vote_accounts)
                .collect();

        let node_keypair_map: HashMap<Pubkey, Keypair> = vote_accounts
            .iter()
            .map(|(_, node_keypair, _, _)| (node_keypair.pubkey(), node_keypair.insecure_clone()))
            .collect();

        let vahm = vote_accounts
            .into_iter()
            .map(|(vote_keypair, _, stake, vote_account)| {
                (vote_keypair.pubkey(), (stake, vote_account))
            })
            .collect();

        (node_keypair_map, vahm)
    }

    #[test_case(1_usize, 0_usize, 10_usize, Protocol::UDP, false)]
    #[test_case(1_usize, 0_usize, 10_usize, Protocol::UDP, true)]
    #[test_case(3_usize, 0_usize, 10_usize, Protocol::QUIC, false)]
    #[test_case(10_usize, 2_usize, 10_usize, Protocol::UDP, false)]
    #[test_case(10_usize, 2_usize, 10_usize, Protocol::UDP, true)]
    #[test_case(10_usize, 10_usize, 10_usize, Protocol::QUIC, false)]
    #[test_case(50_usize, 7_usize, 60_usize, Protocol::UDP, false)]
    #[test_case(50_usize, 7_usize, 60_usize, Protocol::UDP, true)]
    fn test_detect_only_staked_nodes_and_refresh_after_ttl(
        num_nodes: usize,
        num_zero_stake_nodes: usize,
        num_vote_accounts: usize,
        protocol: Protocol,
        use_alpenglow_socket: bool,
    ) {
        let slot_num = 325_000_000_u64;
        let genesis_lamports = 123_u64;
        // Create our harness
        let (keypair_map, vahm) =
            build_epoch_stakes(num_nodes, num_zero_stake_nodes, num_vote_accounts);

        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(genesis_lamports);

        let (bank_forks, cluster_info) =
            StakedValidatorsCacheHarness::new(&genesis_config, Keypair::new())
                .with_vote_accounts(slot_num, keypair_map, vahm, protocol)
                .bank_forks();

        // Create our staked validators cache
        let mut svc =
            StakedValidatorsCache::new(bank_forks, protocol, Duration::from_secs(5), 5, true, None);

        let now = Instant::now();

        let (sockets, refreshed) = if use_alpenglow_socket {
            svc.get_staked_validators_by_slot_with_alpenglow_ports(slot_num, &cluster_info, now)
        } else {
            svc.get_staked_validators_by_slot_with_tpu_vote_ports(slot_num, &cluster_info, now)
        };

        assert!(refreshed);
        assert_eq!(num_nodes - num_zero_stake_nodes, sockets.len());
        assert_eq!(1, svc.len());

        // Re-fetch from the cache right before the 5-second deadline
        let (sockets, refreshed) = if use_alpenglow_socket {
            svc.get_staked_validators_by_slot_with_alpenglow_ports(
                slot_num,
                &cluster_info,
                now + Duration::from_secs_f64(4.999),
            )
        } else {
            svc.get_staked_validators_by_slot_with_tpu_vote_ports(
                slot_num,
                &cluster_info,
                now + Duration::from_secs_f64(4.999),
            )
        };

        assert!(!refreshed);
        assert_eq!(num_nodes - num_zero_stake_nodes, sockets.len());
        assert_eq!(1, svc.len());

        // Re-fetch from the cache right at the 5-second deadline - we still shouldn't refresh.
        let (sockets, refreshed) = if use_alpenglow_socket {
            svc.get_staked_validators_by_slot_with_alpenglow_ports(
                slot_num,
                &cluster_info,
                now + Duration::from_secs(5),
            )
        } else {
            svc.get_staked_validators_by_slot_with_tpu_vote_ports(
                slot_num,
                &cluster_info,
                now + Duration::from_secs(5),
            )
        };

        assert!(!refreshed);
        assert_eq!(num_nodes - num_zero_stake_nodes, sockets.len());
        assert_eq!(1, svc.len());

        // Re-fetch from the cache right after the 5-second deadline - now we should refresh.
        let (sockets, refreshed) = if use_alpenglow_socket {
            svc.get_staked_validators_by_slot_with_alpenglow_ports(
                slot_num,
                &cluster_info,
                now + Duration::from_secs_f64(5.001),
            )
        } else {
            svc.get_staked_validators_by_slot_with_tpu_vote_ports(
                slot_num,
                &cluster_info,
                now + Duration::from_secs_f64(5.001),
            )
        };

        assert!(refreshed);
        assert_eq!(num_nodes - num_zero_stake_nodes, sockets.len());
        assert_eq!(1, svc.len());

        // Re-fetch from the cache well after the 5-second deadline - we should refresh.
        let (sockets, refreshed) = if use_alpenglow_socket {
            svc.get_staked_validators_by_slot_with_alpenglow_ports(
                slot_num,
                &cluster_info,
                now + Duration::from_secs(100),
            )
        } else {
            svc.get_staked_validators_by_slot_with_tpu_vote_ports(
                slot_num,
                &cluster_info,
                now + Duration::from_secs(100),
            )
        };

        assert!(refreshed);
        assert_eq!(num_nodes - num_zero_stake_nodes, sockets.len());
        assert_eq!(1, svc.len());
    }

    #[test_case(true)]
    #[test_case(false)]
    fn test_cache_eviction(use_alpenglow_socket: bool) {
        // Create our harness
        let (keypair_map, vahm) = build_epoch_stakes(50, 7, 60);

        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(123);

        let base_slot = 325_000_000_000;
        let (bank_forks, cluster_info) =
            StakedValidatorsCacheHarness::new(&genesis_config, Keypair::new())
                .with_vote_accounts(base_slot, keypair_map, vahm, Protocol::UDP)
                .bank_forks();

        // Create our staked validators cache
        let mut svc = StakedValidatorsCache::new(
            bank_forks,
            Protocol::UDP,
            Duration::from_secs(5),
            5,
            true,
            None,
        );

        assert_eq!(0, svc.len());
        assert!(svc.is_empty());

        let now = Instant::now();

        // Populate the first five entries; accessing the cache once again shouldn't trigger any
        // refreshes.
        for entry_ix in 1..=5 {
            let (_, refreshed) = if use_alpenglow_socket {
                svc.get_staked_validators_by_slot_with_alpenglow_ports(
                    entry_ix * base_slot,
                    &cluster_info,
                    now,
                )
            } else {
                svc.get_staked_validators_by_slot_with_tpu_vote_ports(
                    entry_ix * base_slot,
                    &cluster_info,
                    now,
                )
            };
            assert!(refreshed);
            assert_eq!(entry_ix as usize, svc.len());

            let (_, refreshed) = if use_alpenglow_socket {
                svc.get_staked_validators_by_slot_with_alpenglow_ports(
                    entry_ix * base_slot,
                    &cluster_info,
                    now,
                )
            } else {
                svc.get_staked_validators_by_slot_with_tpu_vote_ports(
                    entry_ix * base_slot,
                    &cluster_info,
                    now,
                )
            };
            assert!(!refreshed);
            assert_eq!(entry_ix as usize, svc.len());
        }

        // Entry 6 - this shouldn't increase the cache length.
        let (_, refreshed) = if use_alpenglow_socket {
            svc.get_staked_validators_by_slot_with_alpenglow_ports(
                6 * base_slot,
                &cluster_info,
                now,
            )
        } else {
            svc.get_staked_validators_by_slot_with_tpu_vote_ports(6 * base_slot, &cluster_info, now)
        };
        assert!(refreshed);
        assert_eq!(5, svc.len());

        // Epoch 1 should have been evicted
        assert!(!svc.cache.contains(&svc.cur_epoch(base_slot)));

        // Epochs 2 - 6 should have entries
        for entry_ix in 2..=6 {
            assert!(svc.cache.contains(&svc.cur_epoch(entry_ix * base_slot)));
        }

        // Accessing the cache after TTL should recalculate everything; the size remains 5, since
        // we only ever lazily evict cache entries.
        for entry_ix in 1..=5 {
            let (_, refreshed) = if use_alpenglow_socket {
                svc.get_staked_validators_by_slot_with_alpenglow_ports(
                    entry_ix * base_slot,
                    &cluster_info,
                    now + Duration::from_secs(10),
                )
            } else {
                svc.get_staked_validators_by_slot_with_tpu_vote_ports(
                    entry_ix * base_slot,
                    &cluster_info,
                    now + Duration::from_secs(10),
                )
            };
            assert!(refreshed);
            assert_eq!(5, svc.len());
        }
    }

    #[test_case(true)]
    #[test_case(false)]
    fn test_only_update_once_per_epoch(use_alpenglow_socket: bool) {
        let slot_num = 325_000_000_u64;
        let num_nodes = 10_usize;
        let num_zero_stake_nodes = 2_usize;
        let num_vote_accounts = 10_usize;
        let genesis_lamports = 123_u64;
        let protocol = Protocol::UDP;

        // Create our harness
        let (keypair_map, vahm) =
            build_epoch_stakes(num_nodes, num_zero_stake_nodes, num_vote_accounts);

        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(genesis_lamports);

        let (bank_forks, cluster_info) =
            StakedValidatorsCacheHarness::new(&genesis_config, Keypair::new())
                .with_vote_accounts(slot_num, keypair_map, vahm, protocol)
                .bank_forks();

        // Create our staked validators cache
        let mut svc =
            StakedValidatorsCache::new(bank_forks, protocol, Duration::from_secs(5), 5, true, None);

        let now = Instant::now();

        let (_, refreshed) = if use_alpenglow_socket {
            svc.get_staked_validators_by_slot_with_alpenglow_ports(slot_num, &cluster_info, now)
        } else {
            svc.get_staked_validators_by_slot_with_tpu_vote_ports(slot_num, &cluster_info, now)
        };
        assert!(refreshed);

        let (_, refreshed) = if use_alpenglow_socket {
            svc.get_staked_validators_by_slot_with_alpenglow_ports(slot_num, &cluster_info, now)
        } else {
            svc.get_staked_validators_by_slot_with_tpu_vote_ports(slot_num, &cluster_info, now)
        };
        assert!(!refreshed);

        let (_, refreshed) = if use_alpenglow_socket {
            svc.get_staked_validators_by_slot_with_alpenglow_ports(2 * slot_num, &cluster_info, now)
        } else {
            svc.get_staked_validators_by_slot_with_tpu_vote_ports(2 * slot_num, &cluster_info, now)
        };
        assert!(refreshed);
    }

    #[test_matrix(
    [1_usize, 10_usize],
    [Protocol::UDP, Protocol::QUIC],
    [false, true]
)]
    fn test_exclude_self_from_cache(
        num_nodes: usize,
        protocol: Protocol,
        use_alpenglow_socket: bool,
    ) {
        let slot_num = 325_000_000_u64;
        let num_vote_accounts = 10_usize;
        let genesis_lamports = 123_u64;

        // Create our harness
        let (keypair_map, vahm) = build_epoch_stakes(num_nodes, 0, num_vote_accounts);

        // Fetch some keypair from the keypair map
        let keypair = keypair_map.values().next().unwrap().insecure_clone();

        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(genesis_lamports);

        let (bank_forks, cluster_info) =
            StakedValidatorsCacheHarness::new(&genesis_config, keypair.insecure_clone())
                .with_vote_accounts(slot_num, keypair_map, vahm, protocol)
                .bank_forks();

        let my_socket_addr = cluster_info
            .lookup_contact_info(&keypair.pubkey(), |node| {
                if use_alpenglow_socket {
                    node.alpenglow().unwrap()
                } else {
                    node.tpu_vote(protocol).unwrap()
                }
            })
            .unwrap();

        // Create our staked validators cache - set include_self to true
        let mut svc = StakedValidatorsCache::new(
            bank_forks.clone(),
            protocol,
            Duration::from_secs(5),
            5,
            true,
            None,
        );

        let (sockets, _) = if use_alpenglow_socket {
            svc.get_staked_validators_by_slot_with_alpenglow_ports(
                slot_num,
                &cluster_info,
                Instant::now(),
            )
        } else {
            svc.get_staked_validators_by_slot_with_tpu_vote_ports(
                slot_num,
                &cluster_info,
                Instant::now(),
            )
        };
        assert_eq!(sockets.len(), num_nodes);
        assert!(sockets.contains(&my_socket_addr));

        // Create our staked validators cache - set include_self to false
        let mut svc = StakedValidatorsCache::new(
            bank_forks.clone(),
            protocol,
            Duration::from_secs(5),
            5,
            false,
            None,
        );

        let (sockets, _) = if use_alpenglow_socket {
            svc.get_staked_validators_by_slot_with_alpenglow_ports(
                slot_num,
                &cluster_info,
                Instant::now(),
            )
        } else {
            svc.get_staked_validators_by_slot_with_tpu_vote_ports(
                slot_num,
                &cluster_info,
                Instant::now(),
            )
        };
        // We should have num_nodes - 1 sockets, since we exclude our own socket address.
        assert_eq!(sockets.len(), num_nodes - 1);
        assert!(!sockets.contains(&my_socket_addr));
    }

    #[test]
    fn test_alpenglow_port_override() {
        let (keypair_map, vahm) = build_epoch_stakes(3, 0, 3);
        let pubkey_b = *keypair_map.keys().nth(1).unwrap();
        let keypair = keypair_map.values().next().unwrap().insecure_clone();

        let alpenglow_port_override = AlpenglowPortOverride::default();
        let blackhole_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(100);

        let (bank_forks, cluster_info) =
            StakedValidatorsCacheHarness::new(&genesis_config, keypair.insecure_clone())
                .with_vote_accounts(0, keypair_map, vahm, Protocol::UDP)
                .bank_forks();

        // Create our staked validators cache - set include_self to false
        let mut svc = StakedValidatorsCache::new(
            bank_forks.clone(),
            Protocol::UDP,
            Duration::from_secs(5),
            5,
            false,
            Some(alpenglow_port_override.clone()),
        );
        // Nothing in the override, so we should get the original socket addresses.
        let (sockets, _) = svc.get_staked_validators_by_slot_with_alpenglow_ports(
            0,
            &cluster_info,
            Instant::now(),
        );
        assert_eq!(sockets.len(), 2);
        assert!(!sockets.contains(&blackhole_addr));

        // Add an override for pubkey_B, and check that we get the overridden socket address.
        alpenglow_port_override.update_override(HashMap::from([(pubkey_b, blackhole_addr)]));
        let (sockets, _) = svc.get_staked_validators_by_slot_with_alpenglow_ports(
            0,
            &cluster_info,
            Instant::now(),
        );
        assert_eq!(sockets.len(), 2);
        // Sort sockets to ensure the blackhole address is at index 0.
        let mut sockets: Vec<_> = sockets.to_vec();
        sockets.sort();
        assert_eq!(sockets[0], blackhole_addr);
        assert_ne!(sockets[1], blackhole_addr);

        // Now clear the override, and check that we get the original socket addresses.
        alpenglow_port_override.clear();
        let (sockets, _) = svc.get_staked_validators_by_slot_with_alpenglow_ports(
            0,
            &cluster_info,
            Instant::now(),
        );
        assert_eq!(sockets.len(), 2);
        assert!(!sockets.contains(&blackhole_addr));
    }
}
