use {
    crate::stakes::SerdeStakesToStakeFormat,
    serde::{Deserialize, Serialize},
    solana_bls_signatures::{Pubkey as BLSPubkey, PubkeyCompressed as BLSPubkeyCompressed},
    solana_clock::Epoch,
    solana_pubkey::Pubkey,
    solana_vote::vote_account::VoteAccountsHashMap,
    std::{
        collections::HashMap,
        sync::{Arc, OnceLock},
    },
};

pub type NodeIdToVoteAccounts = HashMap<Pubkey, NodeVoteAccounts>;
pub type EpochAuthorizedVoters = HashMap<Pubkey, Pubkey>;

/// Container to store a mapping from validator [`BLSPubkey`] to rank.
///
/// A validator with a smaller rank has a higher stake.
/// Container also supports lookups from rank to [`(Pubkey, BLSPubkey)`].
#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[cfg_attr(feature = "dev-context-only-utils", derive(PartialEq))]
pub struct BLSPubkeyToRankMap {
    /// Mapping from validator [`BLSPubkey`] to rank.
    rank_map: HashMap<BLSPubkey, u16>,
    /// Mapping from rank to validator [`(Pubkey, BLSPubkey)`].
    //
    // TODO(wen): We can make sorted_pubkeys a Vec<BLSPubkey> after we remove ed25519
    // pubkey from the consensus pool.
    sorted_pubkeys: Vec<(Pubkey, BLSPubkey)>,
}

impl BLSPubkeyToRankMap {
    pub fn new(epoch_vote_accounts_hash_map: &VoteAccountsHashMap) -> Self {
        let mut pubkey_stake_pair_vec: Vec<(Pubkey, BLSPubkey, u64)> = epoch_vote_accounts_hash_map
            .iter()
            .filter_map(|(pubkey, (stake, account))| {
                if *stake > 0 {
                    account
                        .vote_state_view()
                        .bls_pubkey_compressed()
                        .and_then(|bls_pubkey_compressed_bytes| {
                            let bls_pubkey_compressed =
                                BLSPubkeyCompressed(bls_pubkey_compressed_bytes);
                            BLSPubkey::try_from(bls_pubkey_compressed).ok()
                        })
                        .map(|bls_pubkey| (*pubkey, bls_pubkey, *stake))
                } else {
                    None
                }
            })
            .collect();
        pubkey_stake_pair_vec.sort_by(|(_, a_pubkey, a_stake), (_, b_pubkey, b_stake)| {
            b_stake.cmp(a_stake).then(a_pubkey.cmp(b_pubkey))
        });
        let mut sorted_pubkeys = Vec::new();
        let mut bls_pubkey_to_rank_map = HashMap::new();
        for (rank, (pubkey, bls_pubkey, _stake)) in pubkey_stake_pair_vec.into_iter().enumerate() {
            sorted_pubkeys.push((pubkey, bls_pubkey));
            bls_pubkey_to_rank_map.insert(bls_pubkey, rank as u16);
        }
        Self {
            rank_map: bls_pubkey_to_rank_map,
            sorted_pubkeys,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rank_map.is_empty()
    }

    pub fn len(&self) -> usize {
        self.rank_map.len()
    }

    pub fn get_rank(&self, bls_pubkey: &BLSPubkey) -> Option<&u16> {
        self.rank_map.get(bls_pubkey)
    }

    pub fn get_pubkey(&self, index: usize) -> Option<&(Pubkey, BLSPubkey)> {
        self.sorted_pubkeys.get(index)
    }
}

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Clone, Serialize, Debug, Deserialize, Default, PartialEq, Eq)]
pub struct NodeVoteAccounts {
    pub vote_accounts: Vec<Pubkey>,
    pub total_stake: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "frozen-abi", derive(AbiExample, AbiEnumVisitor))]
#[cfg_attr(feature = "dev-context-only-utils", derive(PartialEq))]
pub enum VersionedEpochStakes {
    Current {
        stakes: SerdeStakesToStakeFormat,
        /// Total stake in Lamports
        total_stake: u64,
        node_id_to_vote_accounts: Arc<NodeIdToVoteAccounts>,
        epoch_authorized_voters: Arc<EpochAuthorizedVoters>,
        #[serde(skip)]
        bls_pubkey_to_rank_map: OnceLock<Arc<BLSPubkeyToRankMap>>,
    },
}

impl VersionedEpochStakes {
    pub(crate) fn new(stakes: SerdeStakesToStakeFormat, leader_schedule_epoch: Epoch) -> Self {
        let epoch_vote_accounts = stakes.vote_accounts();
        let (total_stake, node_id_to_vote_accounts, epoch_authorized_voters) =
            Self::parse_epoch_vote_accounts(epoch_vote_accounts.as_ref(), leader_schedule_epoch);
        Self::Current {
            stakes,
            total_stake,
            node_id_to_vote_accounts: Arc::new(node_id_to_vote_accounts),
            epoch_authorized_voters: Arc::new(epoch_authorized_voters),
            bls_pubkey_to_rank_map: OnceLock::new(),
        }
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn new_for_tests(
        vote_accounts_hash_map: VoteAccountsHashMap,
        leader_schedule_epoch: Epoch,
    ) -> Self {
        Self::new(
            SerdeStakesToStakeFormat::Account(crate::stakes::Stakes::new_for_tests(
                0,
                solana_vote::vote_account::VoteAccounts::from(Arc::new(vote_accounts_hash_map)),
                im::HashMap::default(),
            )),
            leader_schedule_epoch,
        )
    }

    pub fn stakes(&self) -> &SerdeStakesToStakeFormat {
        match self {
            Self::Current { stakes, .. } => stakes,
        }
    }

    /// Returns the total stake in Lamports.
    pub fn total_stake(&self) -> u64 {
        match self {
            Self::Current { total_stake, .. } => *total_stake,
        }
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn set_total_stake(&mut self, total_stake: u64) {
        match self {
            Self::Current {
                total_stake: total_stake_field,
                ..
            } => {
                *total_stake_field = total_stake;
            }
        }
    }

    pub fn node_id_to_vote_accounts(&self) -> &Arc<NodeIdToVoteAccounts> {
        match self {
            Self::Current {
                node_id_to_vote_accounts,
                ..
            } => node_id_to_vote_accounts,
        }
    }

    pub fn node_id_to_stake(&self, node_id: &Pubkey) -> Option<u64> {
        self.node_id_to_vote_accounts()
            .get(node_id)
            .map(|x| x.total_stake)
    }

    pub fn epoch_authorized_voters(&self) -> &Arc<EpochAuthorizedVoters> {
        match self {
            Self::Current {
                epoch_authorized_voters,
                ..
            } => epoch_authorized_voters,
        }
    }

    pub fn bls_pubkey_to_rank_map(&self) -> &Arc<BLSPubkeyToRankMap> {
        match self {
            Self::Current {
                bls_pubkey_to_rank_map,
                ..
            } => bls_pubkey_to_rank_map.get_or_init(|| {
                Arc::new(BLSPubkeyToRankMap::new(
                    self.stakes().vote_accounts().as_ref(),
                ))
            }),
        }
    }

    /// Returns the stake in Lamports for the given vote_account.
    pub fn vote_account_stake(&self, vote_account: &Pubkey) -> u64 {
        self.stakes()
            .vote_accounts()
            .get_delegated_stake(vote_account)
    }

    fn parse_epoch_vote_accounts(
        epoch_vote_accounts: &VoteAccountsHashMap,
        leader_schedule_epoch: Epoch,
    ) -> (u64, NodeIdToVoteAccounts, EpochAuthorizedVoters) {
        let mut node_id_to_vote_accounts: NodeIdToVoteAccounts = HashMap::new();
        let total_stake = epoch_vote_accounts
            .iter()
            .map(|(_, (stake, _))| stake)
            .sum();
        let epoch_authorized_voters = epoch_vote_accounts
            .iter()
            .filter_map(|(key, (stake, account))| {
                let vote_state = account.vote_state_view();

                if *stake > 0 {
                    if let Some(authorized_voter) =
                        vote_state.get_authorized_voter(leader_schedule_epoch)
                    {
                        let node_vote_accounts = node_id_to_vote_accounts
                            .entry(*vote_state.node_pubkey())
                            .or_default();

                        node_vote_accounts.total_stake += stake;
                        node_vote_accounts.vote_accounts.push(*key);

                        Some((*key, *authorized_voter))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect();
        (
            total_stake,
            node_id_to_vote_accounts,
            epoch_authorized_voters,
        )
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use {
        super::*, solana_account::AccountSharedData,
        solana_bls_signatures::keypair::Keypair as BLSKeypair,
        solana_vote::vote_account::VoteAccount,
        solana_vote_program::vote_state::create_v4_account_with_authorized, std::iter,
        test_case::test_case,
    };

    struct VoteAccountInfo {
        vote_account: Pubkey,
        account: AccountSharedData,
        authorized_voter: Pubkey,
    }

    fn new_vote_accounts(
        num_nodes: usize,
        num_vote_accounts_per_node: usize,
        is_alpenglow: bool,
    ) -> HashMap<Pubkey, Vec<VoteAccountInfo>> {
        // Create some vote accounts for each pubkey
        (0..num_nodes)
            .map(|_| {
                let node_id = solana_pubkey::new_rand();
                (
                    node_id,
                    iter::repeat_with(|| {
                        let authorized_voter = solana_pubkey::new_rand();
                        let bls_pubkey_compressed: BLSPubkeyCompressed =
                            BLSKeypair::new().public.try_into().unwrap();
                        let bls_pubkey_compressed_serialized =
                            bincode::serialize(&bls_pubkey_compressed)
                                .unwrap()
                                .try_into()
                                .unwrap();

                        let account = if is_alpenglow {
                            create_v4_account_with_authorized(
                                &node_id,
                                &authorized_voter,
                                &node_id,
                                Some(bls_pubkey_compressed_serialized),
                                0,
                                100,
                            )
                        } else {
                            create_v4_account_with_authorized(
                                &node_id,
                                &authorized_voter,
                                &node_id,
                                None,
                                0,
                                100,
                            )
                        };
                        VoteAccountInfo {
                            vote_account: solana_pubkey::new_rand(),
                            account,
                            authorized_voter,
                        }
                    })
                    .take(num_vote_accounts_per_node)
                    .collect(),
                )
            })
            .collect()
    }

    fn new_epoch_vote_accounts(
        vote_accounts_map: &HashMap<Pubkey, Vec<VoteAccountInfo>>,
        node_id_to_stake_fn: impl Fn(&Pubkey) -> u64,
    ) -> VoteAccountsHashMap {
        // Create and process the vote accounts
        vote_accounts_map
            .iter()
            .flat_map(|(node_id, vote_accounts)| {
                vote_accounts.iter().map(|v| {
                    let vote_account = VoteAccount::try_from(v.account.clone()).unwrap();
                    (v.vote_account, (node_id_to_stake_fn(node_id), vote_account))
                })
            })
            .collect()
    }

    #[test_case(true; "alpenglow")]
    #[test_case(false; "towerbft")]
    fn test_parse_epoch_vote_accounts(is_alpenglow: bool) {
        let stake_per_account = 100;
        let num_vote_accounts_per_node = 2;
        let num_nodes = 10;

        let vote_accounts_map =
            new_vote_accounts(num_nodes, num_vote_accounts_per_node, is_alpenglow);

        let expected_authorized_voters: HashMap<_, _> = vote_accounts_map
            .iter()
            .flat_map(|(_, vote_accounts)| {
                vote_accounts
                    .iter()
                    .map(|v| (v.vote_account, v.authorized_voter))
            })
            .collect();

        let expected_node_id_to_vote_accounts: HashMap<_, _> = vote_accounts_map
            .iter()
            .map(|(node_pubkey, vote_accounts)| {
                let mut vote_accounts = vote_accounts
                    .iter()
                    .map(|v| v.vote_account)
                    .collect::<Vec<_>>();
                vote_accounts.sort();
                let node_vote_accounts = NodeVoteAccounts {
                    vote_accounts,
                    total_stake: stake_per_account * num_vote_accounts_per_node as u64,
                };
                (*node_pubkey, node_vote_accounts)
            })
            .collect();

        let epoch_vote_accounts =
            new_epoch_vote_accounts(&vote_accounts_map, |_| stake_per_account);

        let (total_stake, mut node_id_to_vote_accounts, epoch_authorized_voters) =
            VersionedEpochStakes::parse_epoch_vote_accounts(&epoch_vote_accounts, 0);

        // Verify the results
        node_id_to_vote_accounts
            .iter_mut()
            .for_each(|(_, node_vote_accounts)| node_vote_accounts.vote_accounts.sort());

        assert!(
            node_id_to_vote_accounts.len() == expected_node_id_to_vote_accounts.len()
                && node_id_to_vote_accounts
                    .iter()
                    .all(|(k, v)| expected_node_id_to_vote_accounts.get(k).unwrap() == v)
        );
        assert!(
            epoch_authorized_voters.len() == expected_authorized_voters.len()
                && epoch_authorized_voters
                    .iter()
                    .all(|(k, v)| expected_authorized_voters.get(k).unwrap() == v)
        );
        assert_eq!(
            total_stake,
            num_nodes as u64 * num_vote_accounts_per_node as u64 * 100
        );
    }

    #[test_case(true; "alpenglow")]
    #[test_case(false; "towerbft")]
    fn test_node_id_to_stake(is_alpenglow: bool) {
        let num_nodes = 10;
        let num_vote_accounts_per_node = 2;

        let vote_accounts_map =
            new_vote_accounts(num_nodes, num_vote_accounts_per_node, is_alpenglow);
        let node_id_to_stake_map = vote_accounts_map
            .keys()
            .enumerate()
            .map(|(index, node_id)| (*node_id, ((index + 1) * 100) as u64))
            .collect::<HashMap<_, _>>();
        let epoch_vote_accounts = new_epoch_vote_accounts(&vote_accounts_map, |node_id| {
            *node_id_to_stake_map.get(node_id).unwrap()
        });
        let epoch_stakes = VersionedEpochStakes::new_for_tests(epoch_vote_accounts, 0);

        assert_eq!(epoch_stakes.total_stake(), 11000);
        for (node_id, stake) in node_id_to_stake_map.iter() {
            assert_eq!(
                epoch_stakes.node_id_to_stake(node_id),
                Some(*stake * num_vote_accounts_per_node as u64)
            );
        }
    }

    #[test_case(1; "single_vote_account")]
    #[test_case(2; "multiple_vote_accounts")]
    fn test_bls_pubkey_rank_map(num_vote_accounts_per_node: usize) {
        agave_logger::setup();
        let num_nodes = 10;
        let num_vote_accounts = num_nodes * num_vote_accounts_per_node;

        let vote_accounts_map = new_vote_accounts(num_nodes, num_vote_accounts_per_node, true);
        let node_id_to_stake_map = vote_accounts_map
            .keys()
            .enumerate()
            .map(|(index, node_id)| (*node_id, ((index + 1) * 100) as u64))
            .collect::<HashMap<_, _>>();
        let epoch_vote_accounts = new_epoch_vote_accounts(&vote_accounts_map, |node_id| {
            *node_id_to_stake_map.get(node_id).unwrap()
        });
        let epoch_stakes = VersionedEpochStakes::new_for_tests(epoch_vote_accounts.clone(), 0);
        let bls_pubkey_to_rank_map = epoch_stakes.bls_pubkey_to_rank_map();
        assert_eq!(bls_pubkey_to_rank_map.len(), num_vote_accounts);
        for (pubkey, (_, vote_account)) in epoch_vote_accounts {
            let vote_state_view = vote_account.vote_state_view();
            let bls_pubkey_compressed = bincode::deserialize::<BLSPubkeyCompressed>(
                &vote_state_view.bls_pubkey_compressed().unwrap(),
            )
            .unwrap();
            let bls_pubkey = BLSPubkey::try_from(bls_pubkey_compressed).unwrap();
            let index = bls_pubkey_to_rank_map.get_rank(&bls_pubkey).unwrap();
            assert!(index >= &0 && index < &(num_vote_accounts as u16));
            assert_eq!(
                bls_pubkey_to_rank_map.get_pubkey(*index as usize),
                Some(&(pubkey, bls_pubkey))
            );
        }

        // Convert it to versioned and back, we should get the same rank map
        let mut bank_epoch_stakes = HashMap::new();
        bank_epoch_stakes.insert(0, epoch_stakes.clone());
        let epoch_stakes = bank_epoch_stakes
            .get(&0)
            .expect("Epoch stakes should exist");
        let bls_pubkey_to_rank_map2 = epoch_stakes.bls_pubkey_to_rank_map();
        assert_eq!(bls_pubkey_to_rank_map2, bls_pubkey_to_rank_map);
    }
}
