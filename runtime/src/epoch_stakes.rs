#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    crate::{stake_history::StakeHistory, stakes::SerdeStakesToStakeFormat},
    serde::{
        Deserialize, Deserializer, Serialize, Serializer,
        de::{SeqAccess, Visitor},
    },
    solana_bls_signatures::pubkey::{
        PubkeyAffine as BLSPubkeyAffine, PubkeyCompressed as BLSPubkeyCompressed,
    },
    solana_clock::Epoch,
    solana_pubkey::Pubkey,
    solana_stake_interface::state::Stake,
    solana_vote::vote_account::{VoteAccounts, VoteAccountsHashMap},
    solana_vote_interface::state::BLS_PUBLIC_KEY_COMPRESSED_SIZE,
    std::{
        collections::HashMap,
        fmt,
        sync::{Arc, OnceLock},
    },
};

pub type NodeIdToVoteAccounts = HashMap<Pubkey, NodeVoteAccounts>;
pub type EpochAuthorizedVoters = HashMap<Pubkey, Pubkey>;

/// Entry in the [`BLSPubkeyToRankMap`] associating a validator's identity
/// pubkey and BLS pubkey with its stake.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "dev-context-only-utils", derive(PartialEq))]
pub struct BLSPubkeyStakeEntry {
    /// The address containing the vote account
    pub vote_account_pubkey: Pubkey,
    /// The identity of the validator specified in the vote account
    pub node_pubkey: Pubkey,
    /// The bls pubkey of the validator specified in the vote account
    pub bls_pubkey: BLSPubkeyAffine,
    /// The stake of the validator
    pub stake: u64,
}

/// Container to store a mapping from validator [`BLSPubkeyAffine`] to rank.
///
/// A validator with a smaller rank has a higher stake.
/// Container also supports lookups from rank to [`BLSPubkeyStakeEntry`].
#[derive(Clone, Debug)]
#[cfg_attr(feature = "dev-context-only-utils", derive(PartialEq))]
pub struct BLSPubkeyToRankMap {
    rank_map: HashMap<BLSPubkeyCompressed, u16>,
    vote_pubkey_to_rank: HashMap<Pubkey, u16>,
    sorted_pubkeys: Vec<BLSPubkeyStakeEntry>,
}

// We cannot auto derive `AbiExample` for `BLSPubkeyToRankMap` because
// the `BLSPubkeyAffine` type does not implement `AbiExample` or `Default`.
#[cfg(feature = "frozen-abi")]
impl solana_frozen_abi::abi_example::AbiExample for BLSPubkeyToRankMap {
    fn example() -> Self {
        Self {
            rank_map: HashMap::new(),
            vote_pubkey_to_rank: HashMap::new(),
            sorted_pubkeys: Vec::new(),
        }
    }
}

pub(crate) fn bls_pubkey_compressed_bytes_to_bls_pubkey(
    bls_pubkey_compressed_bytes: [u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE],
) -> Option<(BLSPubkeyCompressed, BLSPubkeyAffine)> {
    let bls_pubkey_compressed: BLSPubkeyCompressed =
        bincode::deserialize(&bls_pubkey_compressed_bytes).ok()?;
    let bls_pubkey_affine = BLSPubkeyAffine::try_from(bls_pubkey_compressed).ok()?;
    Some((bls_pubkey_compressed, bls_pubkey_affine))
}

impl BLSPubkeyToRankMap {
    pub fn new(epoch_vote_accounts_hash_map: &VoteAccountsHashMap) -> Self {
        let mut keys_stake_entry_with_compressed: Vec<(BLSPubkeyStakeEntry, BLSPubkeyCompressed)> =
            epoch_vote_accounts_hash_map
                .iter()
                .filter_map(|(&vote_account_pubkey, (stake, account))| {
                    if *stake > 0 {
                        let node_pubkey = *account.vote_state_view().node_pubkey();
                        account
                            .vote_state_view()
                            .bls_pubkey_compressed()
                            .and_then(bls_pubkey_compressed_bytes_to_bls_pubkey)
                            .map(|(bls_pubkey_compressed, bls_pubkey)| {
                                (
                                    BLSPubkeyStakeEntry {
                                        vote_account_pubkey,
                                        node_pubkey,
                                        bls_pubkey,
                                        stake: *stake,
                                    },
                                    bls_pubkey_compressed,
                                )
                            })
                    } else {
                        None
                    }
                })
                .collect();
        keys_stake_entry_with_compressed.sort_by(
            |(a_entry, a_pubkey_compressed), (b_entry, b_pubkey_compressed)| {
                b_entry
                    .stake
                    .cmp(&a_entry.stake)
                    .then(a_pubkey_compressed.cmp(b_pubkey_compressed))
            },
        );
        let mut sorted_pubkeys = Vec::with_capacity(keys_stake_entry_with_compressed.len());
        let mut bls_pubkey_to_rank_map =
            HashMap::with_capacity(keys_stake_entry_with_compressed.len());
        let mut vote_pubkey_to_rank_map =
            HashMap::with_capacity(keys_stake_entry_with_compressed.len());
        for (rank, (entry, bls_pubkey_compressed)) in
            keys_stake_entry_with_compressed.into_iter().enumerate()
        {
            vote_pubkey_to_rank_map.insert(entry.vote_account_pubkey, rank as u16);
            bls_pubkey_to_rank_map.insert(bls_pubkey_compressed, rank as u16);
            sorted_pubkeys.push(entry);
        }
        Self {
            rank_map: bls_pubkey_to_rank_map,
            vote_pubkey_to_rank: vote_pubkey_to_rank_map,
            sorted_pubkeys,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rank_map.is_empty()
    }

    pub fn len(&self) -> usize {
        self.rank_map.len()
    }

    pub fn get_rank(&self, bls_pubkey: &BLSPubkeyAffine) -> Option<&u16> {
        let bls_pubkey_compressed = BLSPubkeyCompressed(bls_pubkey.to_bytes_compressed());
        self.rank_map.get(&bls_pubkey_compressed)
    }

    pub fn get_rank_for_vote_pubkey(&self, vote_pubkey: &Pubkey) -> Option<&u16> {
        self.vote_pubkey_to_rank.get(vote_pubkey)
    }

    pub fn get_pubkey_stake_entry(&self, index: usize) -> Option<&BLSPubkeyStakeEntry> {
        self.sorted_pubkeys.get(index)
    }
}

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Clone, Serialize, Debug, Deserialize, Default, PartialEq, Eq)]
pub struct NodeVoteAccounts {
    pub vote_accounts: Vec<Pubkey>,
    pub total_stake: u64,
}

/// Simplified, intermediate representation of [`VersionedEpochStakes`]
///
/// Its bincode serializaiton format is identical as `VersionedEpochStakes`, but allows faster
/// deserialization by ignoring serialized stake delegations entirely.
#[derive(Clone, Debug, Deserialize)]
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
pub(crate) enum DeserializableVersionedEpochStakes {
    Current {
        stakes: DeserializableEpochStakes,
        total_stake: u64,
        node_id_to_vote_accounts: NodeIdToVoteAccounts,
        epoch_authorized_voters: EpochAuthorizedVoters,
    },
}

#[derive(Clone, Debug, Serialize)]
#[cfg_attr(feature = "frozen-abi", derive(AbiExample, AbiEnumVisitor))]
#[cfg_attr(feature = "dev-context-only-utils", derive(PartialEq))]
pub enum VersionedEpochStakes {
    Current {
        stakes: EpochStakes,
        /// Total stake in Lamports
        total_stake: u64,
        node_id_to_vote_accounts: Arc<NodeIdToVoteAccounts>,
        epoch_authorized_voters: Arc<EpochAuthorizedVoters>,
        #[serde(skip)]
        bls_pubkey_to_rank_map: OnceLock<Arc<BLSPubkeyToRankMap>>,
    },
}

impl From<DeserializableVersionedEpochStakes> for VersionedEpochStakes {
    fn from(epoch_stakes: DeserializableVersionedEpochStakes) -> Self {
        let DeserializableVersionedEpochStakes::Current {
            stakes,
            total_stake,
            node_id_to_vote_accounts,
            epoch_authorized_voters,
        } = epoch_stakes;
        Self::Current {
            stakes: stakes.into(),
            total_stake,
            node_id_to_vote_accounts: Arc::new(node_id_to_vote_accounts),
            epoch_authorized_voters: Arc::new(epoch_authorized_voters),
            bls_pubkey_to_rank_map: OnceLock::new(),
        }
    }
}

impl VersionedEpochStakes {
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn new(stakes: SerdeStakesToStakeFormat, leader_schedule_epoch: Epoch) -> Self {
        let stakes = EpochStakes::from(stakes);
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
                imbl::HashMap::default(),
            )),
            leader_schedule_epoch,
        )
    }

    pub fn stakes(&self) -> &EpochStakes {
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
        let mut epoch_authorized_voters: EpochAuthorizedVoters = HashMap::new();
        let mut total_stake: u64 = 0;

        for (key, (stake, account)) in epoch_vote_accounts.iter() {
            total_stake += *stake;

            if *stake == 0 {
                continue;
            }

            let vote_state = account.vote_state_view();

            if let Some(authorized_voter) = vote_state.get_authorized_voter(leader_schedule_epoch) {
                let node_vote_accounts = node_id_to_vote_accounts
                    .entry(*vote_state.node_pubkey())
                    .or_default();

                node_vote_accounts.total_stake += stake;
                node_vote_accounts.vote_accounts.push(*key);

                epoch_authorized_voters.insert(*key, *authorized_voter);
            }
        }

        (
            total_stake,
            node_id_to_vote_accounts,
            epoch_authorized_voters,
        )
    }
}

/// The current version of epoch stakes
#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[cfg_attr(feature = "dev-context-only-utils", derive(PartialEq))]
pub struct EpochStakes {
    epoch: Epoch,
    vote_accounts: VoteAccounts,
    stake_history: StakeHistory,
}

impl EpochStakes {
    pub fn vote_accounts(&self) -> &VoteAccounts {
        &self.vote_accounts
    }
    pub fn staked_nodes(&self) -> Arc<HashMap<Pubkey, u64>> {
        self.vote_accounts.staked_nodes()
    }
}

/// Customization of EpochStakes for snapshot serialization.
///
/// Needed because snapshots require additional fields no longer present in EpochStakes.
impl Serialize for EpochStakes {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        #[derive(Serialize)]
        struct SerializableEpochStakes<'a> {
            vote_accounts: &'a VoteAccounts,
            stake_delegations: Vec<(Pubkey, Stake)>,
            unused: u64,
            epoch: Epoch,
            stake_history: &'a StakeHistory,
        }

        SerializableEpochStakes {
            vote_accounts: &self.vote_accounts,
            stake_delegations: Vec::new(), // do not serialize any stake delegations
            unused: 0,
            epoch: self.epoch,
            stake_history: &self.stake_history,
        }
        .serialize(serializer)
    }
}

impl From<SerdeStakesToStakeFormat> for EpochStakes {
    fn from(stakes: SerdeStakesToStakeFormat) -> Self {
        let (epoch, vote_accounts, stake_history) = match stakes {
            SerdeStakesToStakeFormat::Stake(stakes) => stakes.into_epoch_stakes_fields(),
            SerdeStakesToStakeFormat::Account(stakes) => stakes.into_epoch_stakes_fields(),
        };
        Self {
            epoch,
            vote_accounts,
            stake_history,
        }
    }
}

/// Customization of EpochStakes for snapshot deserialization.
///
/// Needed because snapshots contain additional fields no longer present in EpochStakes.
#[derive(Clone, Debug, Deserialize)]
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
pub(crate) struct DeserializableEpochStakes {
    vote_accounts: VoteAccounts,
    #[serde(deserialize_with = "deserialize_and_ignore_stake_delegations")]
    _stake_delegations: (),
    _unused: u64,
    epoch: Epoch,
    stake_history: StakeHistory,
}

impl From<DeserializableEpochStakes> for EpochStakes {
    fn from(stakes: DeserializableEpochStakes) -> Self {
        let DeserializableEpochStakes {
            vote_accounts,
            _stake_delegations: _,
            _unused: _,
            epoch,
            stake_history,
        } = stakes;
        Self {
            epoch,
            vote_accounts,
            stake_history,
        }
    }
}

/// Snapshot epoch stakes contain delegations, but the main EpochStakes no longer uses them.
/// This fn does custom deserialization to visit-and-ignore the delegations,
/// avoiding the need to construct an expensive imbl::HashMap.
fn deserialize_and_ignore_stake_delegations<'de, D>(deserializer: D) -> Result<(), D::Error>
where
    D: Deserializer<'de>,
{
    struct IgnoredStakeDelegationsVisitor;

    impl<'de> Visitor<'de> for IgnoredStakeDelegationsVisitor {
        type Value = ();

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a sequence of serialized stake delegations")
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            while seq.next_element::<(Pubkey, Stake)>()?.is_some() {
                // nothing to do here, ignore the delegations
            }
            Ok(())
        }
    }

    deserializer.deserialize_seq(IgnoredStakeDelegationsVisitor)
}

#[cfg(test)]
pub(crate) mod tests {
    use {
        super::*,
        crate::{stake_account::StakeAccount, stakes::Stakes},
        solana_account::AccountSharedData,
        solana_bls_signatures::keypair::Keypair as BLSKeypair,
        solana_rent::Rent,
        solana_vote::vote_account::VoteAccount,
        solana_vote_program::vote_state::create_v4_account_with_authorized,
        std::iter,
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
                            BLSKeypair::new().public.into();
                        let bls_pubkey_compressed_serialized =
                            bincode::serialize(&bls_pubkey_compressed)
                                .unwrap()
                                .try_into()
                                .unwrap();

                        let bls_pubkey = if is_alpenglow {
                            bls_pubkey_compressed_serialized
                        } else {
                            [0u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE]
                        };
                        let account = create_v4_account_with_authorized(
                            &node_id,
                            &authorized_voter,
                            bls_pubkey,
                            &node_id,
                            0,
                            &node_id,
                            0,
                            &node_id,
                            100,
                        );
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
        for (vote_account_pubkey, (stake, vote_account)) in epoch_vote_accounts {
            let vote_state_view = vote_account.vote_state_view();
            let (_comp, bls_pubkey) = bls_pubkey_compressed_bytes_to_bls_pubkey(
                vote_state_view.bls_pubkey_compressed().unwrap(),
            )
            .unwrap();
            let node_pubkey = *vote_state_view.node_pubkey();
            let index = bls_pubkey_to_rank_map.get_rank(&bls_pubkey).unwrap();
            assert!(index >= &0 && index < &(num_vote_accounts as u16));
            assert_eq!(
                bls_pubkey_to_rank_map.get_pubkey_stake_entry(*index as usize),
                Some(&BLSPubkeyStakeEntry {
                    vote_account_pubkey,
                    node_pubkey,
                    bls_pubkey,
                    stake,
                })
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

    #[test]
    fn test_versioned_epoch_stakes_does_not_serialize_delegations() {
        // test-only types to get the serialized EpochStakes that still have stake delegations
        #[derive(Deserialize)]
        enum SerializedVersionedEpochStakes {
            Current {
                stakes: SerializedEpochStakes,
                total_stake: u64,
                node_id_to_vote_accounts: NodeIdToVoteAccounts,
                epoch_authorized_voters: EpochAuthorizedVoters,
            },
        }
        #[derive(Deserialize)]
        struct SerializedEpochStakes {
            vote_accounts: VoteAccounts,
            stake_delegations: Vec<(Pubkey, Stake)>,
            unused: u64,
            epoch: Epoch,
            stake_history: StakeHistory,
        }

        let epoch = 42;
        let delegated_amount = 456_789;
        let rent = Rent::default();
        let ((vote_pubkey, vote_account), (stake_pubkey, stake_account)) =
            crate::stakes::tests::create_staked_node_accounts(123, &rent);
        let vote_account = VoteAccount::try_from(vote_account).unwrap();
        let vote_accounts = VoteAccounts::from(Arc::new(HashMap::from([(
            vote_pubkey,
            (delegated_amount, vote_account),
        )])));
        let stake_account = StakeAccount::try_from(stake_account).unwrap();
        let stakes = Stakes::new_for_tests(
            epoch,
            vote_accounts,
            imbl::HashMap::from_iter([(stake_pubkey, stake_account)]),
        );

        // ensure stake delegations start off *not* empty
        assert!(!stakes.stake_delegations().is_empty());

        let epoch_stakes = VersionedEpochStakes::new(SerdeStakesToStakeFormat::Account(stakes), 0);

        assert_eq!(
            epoch_stakes
                .stakes()
                .vote_accounts()
                .get_delegated_stake(&vote_pubkey),
            delegated_amount,
        );

        let serialized_bytes = bincode::serialize(&epoch_stakes).unwrap();
        let serialized_epoch_stakes: SerializedVersionedEpochStakes =
            bincode::deserialize(&serialized_bytes).unwrap();
        match serialized_epoch_stakes {
            SerializedVersionedEpochStakes::Current {
                stakes,
                total_stake,
                node_id_to_vote_accounts,
                epoch_authorized_voters,
            } => {
                assert_eq!(
                    stakes.vote_accounts.get_delegated_stake(&vote_pubkey),
                    delegated_amount,
                );
                assert!(stakes.stake_delegations.is_empty()); // delegations are *not* serialized
                assert_eq!(stakes.unused, 0);
                assert_eq!(stakes.epoch, epoch);
                assert_eq!(stakes.stake_history, StakeHistory::default());
                assert_eq!(total_stake, delegated_amount);
                assert_eq!(node_id_to_vote_accounts.len(), 1);
                assert_eq!(epoch_authorized_voters.len(), 1);
            }
        }

        let deserialized_epoch_stakes: VersionedEpochStakes =
            bincode::deserialize::<DeserializableVersionedEpochStakes>(&serialized_bytes)
                .unwrap()
                .into();
        assert_eq!(
            deserialized_epoch_stakes
                .stakes()
                .vote_accounts()
                .get_delegated_stake(&vote_pubkey),
            delegated_amount,
        );
    }
}
