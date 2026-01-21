use {
    super::{StakeAccount, Stakes},
    crate::stake_history::StakeHistory,
    im::HashMap as ImHashMap,
    serde::{ser::SerializeMap, Deserialize, Serialize, Serializer},
    solana_clock::Epoch,
    solana_pubkey::Pubkey,
    solana_stake_interface::state::Stake,
    solana_vote::vote_account::VoteAccounts,
    std::{collections::HashMap, sync::Arc},
};

/// Wrapper struct with custom serialization to support serializing
/// `Stakes<StakeAccount>` as `Stakes<Stake>` without doing an intermediate
/// clone of the stake data.
#[cfg_attr(feature = "frozen-abi", derive(AbiExample, AbiEnumVisitor))]
#[derive(Debug, Clone)]
pub enum SerdeStakesToStakeFormat {
    Stake(Stakes<Stake>),
    Account(Stakes<StakeAccount>),
}

impl SerdeStakesToStakeFormat {
    pub fn vote_accounts(&self) -> &VoteAccounts {
        match self {
            Self::Stake(stakes) => stakes.vote_accounts(),
            Self::Account(stakes) => stakes.vote_accounts(),
        }
    }

    pub fn staked_nodes(&self) -> Arc<HashMap<Pubkey, u64>> {
        match self {
            Self::Stake(stakes) => stakes.staked_nodes(),
            Self::Account(stakes) => stakes.staked_nodes(),
        }
    }
}

#[cfg(feature = "dev-context-only-utils")]
impl PartialEq<Self> for SerdeStakesToStakeFormat {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Stake(stakes), Self::Stake(other)) => stakes == other,
            (Self::Account(stakes), Self::Account(other)) => stakes == other,
            (Self::Stake(stakes), Self::Account(other)) => {
                stakes == &Stakes::<Stake>::from(other.clone())
            }
            (Self::Account(stakes), Self::Stake(other)) => {
                other == &Stakes::<Stake>::from(stakes.clone())
            }
        }
    }
}

impl From<Stakes<StakeAccount>> for SerdeStakesToStakeFormat {
    fn from(stakes: Stakes<StakeAccount>) -> Self {
        Self::Account(stakes)
    }
}

impl Serialize for SerdeStakesToStakeFormat {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Stake(stakes) => stakes.serialize(serializer),
            Self::Account(stakes) => serialize_stake_accounts_to_stake_format(stakes, serializer),
        }
    }
}

pub(crate) fn serialize_stake_accounts_to_delegation_format<S: Serializer>(
    stakes: &Stakes<StakeAccount>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    SerdeStakeAccountsToDelegationFormat::from(stakes.clone()).serialize(serializer)
}

fn serialize_stake_accounts_to_stake_format<S: Serializer>(
    stakes: &Stakes<StakeAccount>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    SerdeStakeAccountsToStakeFormat::from(stakes.clone()).serialize(serializer)
}

impl From<Stakes<StakeAccount>> for SerdeStakeAccountsToDelegationFormat {
    fn from(stakes: Stakes<StakeAccount>) -> Self {
        let Stakes {
            vote_accounts,
            stake_delegations,
            unused,
            epoch,
            stake_history,
        } = stakes;

        Self {
            vote_accounts,
            stake_delegations: SerdeStakeAccountMapToDelegationFormat(stake_delegations),
            unused,
            epoch,
            stake_history,
        }
    }
}

impl From<Stakes<StakeAccount>> for SerdeStakeAccountsToStakeFormat {
    fn from(stakes: Stakes<StakeAccount>) -> Self {
        let Stakes {
            vote_accounts,
            stake_delegations,
            unused,
            epoch,
            stake_history,
        } = stakes;

        Self {
            vote_accounts,
            stake_delegations: SerdeStakeAccountMapToStakeFormat(stake_delegations),
            unused,
            epoch,
            stake_history,
        }
    }
}

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Serialize)]
struct SerdeStakeAccountsToDelegationFormat {
    vote_accounts: VoteAccounts,
    stake_delegations: SerdeStakeAccountMapToDelegationFormat,
    unused: u64,
    epoch: Epoch,
    stake_history: StakeHistory,
}

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Serialize)]
struct SerdeStakeAccountsToStakeFormat {
    vote_accounts: VoteAccounts,
    stake_delegations: SerdeStakeAccountMapToStakeFormat,
    unused: u64,
    epoch: Epoch,
    stake_history: StakeHistory,
}

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
struct SerdeStakeAccountMapToDelegationFormat(ImHashMap<Pubkey, StakeAccount>);
impl Serialize for SerdeStakeAccountMapToDelegationFormat {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut s = serializer.serialize_map(Some(self.0.len()))?;
        for (pubkey, stake_account) in self.0.iter() {
            s.serialize_entry(pubkey, stake_account.delegation())?;
        }
        s.end()
    }
}

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
struct SerdeStakeAccountMapToStakeFormat(ImHashMap<Pubkey, StakeAccount>);
impl Serialize for SerdeStakeAccountMapToStakeFormat {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut s = serializer.serialize_map(Some(self.0.len()))?;
        for (pubkey, stake_account) in self.0.iter() {
            s.serialize_entry(pubkey, stake_account.stake())?;
        }
        s.end()
    }
}

/// Simplified, intermediate representation of [`Stakes<T>`]
///
/// Its bincode serializaiton format is identical as Stakes<T>, but allows faster
/// deserialization without creating im::HashMap (such conversion is deferred until
/// data is actually needed).
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct DeserializableStakes<T> {
    pub vote_accounts: VoteAccounts,
    pub stake_delegations: Vec<(Pubkey, T)>,
    pub unused: u64,
    pub epoch: Epoch,
    pub stake_history: StakeHistory,
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{stake_utils, stakes::StakesCache},
        rand::Rng,
        serde::Deserialize,
        solana_rent::Rent,
        solana_stake_interface::state::Delegation,
        solana_vote_interface::state::BLS_PUBLIC_KEY_COMPRESSED_SIZE,
        solana_vote_program::vote_state,
    };

    #[test]
    fn test_serde_stakes_to_stake_format() {
        let mut stake_delegations = ImHashMap::new();
        let vote_pubkey = Pubkey::new_unique();
        let node_pubkey = Pubkey::new_unique();
        stake_delegations.insert(
            Pubkey::new_unique(),
            StakeAccount::try_from(stake_utils::create_stake_account(
                &Pubkey::new_unique(),
                &vote_pubkey,
                &vote_state::create_v4_account_with_authorized(
                    &node_pubkey,
                    &vote_pubkey,
                    [0u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE],
                    &vote_pubkey,
                    0,
                    &vote_pubkey,
                    0,
                    &vote_pubkey,
                    1_000_000_000,
                ),
                &Rent::default(),
                1_000_000_000,
            ))
            .unwrap(),
        );

        let stake_account_stakes = Stakes {
            vote_accounts: VoteAccounts::default(),
            stake_delegations,
            unused: 0,
            epoch: 0,
            stake_history: StakeHistory::default(),
        };

        let wrapped_stakes = SerdeStakesToStakeFormat::Account(stake_account_stakes.clone());
        let serialized_stakes = bincode::serialize(&wrapped_stakes).unwrap();
        let stake_stakes = Stakes::from_deserialized(
            bincode::deserialize::<DeserializableStakes<Stake>>(&serialized_stakes).unwrap(),
        );
        let expected_stake_stakes = Stakes::<Stake>::from(stake_account_stakes);
        assert_eq!(expected_stake_stakes, stake_stakes);
    }

    #[test]
    fn test_serde_stakes_to_delegation_format() {
        #[derive(Debug, Serialize)]
        struct SerializableDummy {
            head: String,
            #[serde(serialize_with = "serialize_stake_accounts_to_delegation_format")]
            stakes: Stakes<StakeAccount>,
            tail: String,
        }

        #[derive(Debug, Deserialize)]
        struct DeserializableDummy {
            head: String,
            stakes: DeserializableStakes<Delegation>,
            tail: String,
        }

        let mut rng = rand::rng();
        let stakes_cache = StakesCache::new(Stakes {
            unused: rng.random(),
            epoch: rng.random(),
            ..Stakes::default()
        });
        for _ in 0..rng.random_range(5usize..10) {
            let vote_pubkey = solana_pubkey::new_rand();
            let node_pubkey = solana_pubkey::new_rand();
            let commission = rng.random_range(0..101);
            let commission_bps = commission * 100;
            let vote_account = vote_state::create_v4_account_with_authorized(
                &node_pubkey,
                &vote_pubkey,
                [0u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE],
                &vote_pubkey,
                commission_bps,
                &vote_pubkey,
                0,
                &vote_pubkey,
                rng.random_range(0..1_000_000), // lamports
            );
            stakes_cache.check_and_store(&vote_pubkey, &vote_account, None);
            for _ in 0..rng.random_range(10usize..20) {
                let stake_pubkey = solana_pubkey::new_rand();
                let rent = Rent::with_slots_per_epoch(rng.random());
                let stake_account = stake_utils::create_stake_account(
                    &stake_pubkey, // authorized
                    &vote_pubkey,
                    &vote_account,
                    &rent,
                    rng.random_range(0..1_000_000), // lamports
                );
                stakes_cache.check_and_store(&stake_pubkey, &stake_account, None);
            }
        }
        let stakes: Stakes<StakeAccount> = stakes_cache.stakes().clone();
        assert!(stakes.vote_accounts.as_ref().len() >= 5);
        assert!(stakes.stake_delegations.len() >= 50);
        let dummy = SerializableDummy {
            head: String::from("dummy-head"),
            stakes: stakes.clone(),
            tail: String::from("dummy-tail"),
        };
        assert!(dummy.stakes.vote_accounts().as_ref().len() >= 5);
        let data = bincode::serialize(&dummy).unwrap();
        let other: DeserializableDummy = bincode::deserialize(&data).unwrap();
        assert_eq!(other.head, dummy.head);
        assert_eq!(other.tail, dummy.tail);

        assert!(other.stakes.vote_accounts.as_ref().len() >= 5);
        assert_eq!(other.stakes.vote_accounts, stakes.vote_accounts);

        assert_eq!(other.stakes.epoch, stakes.epoch);
        assert_eq!(other.stakes.stake_history, stakes.stake_history);

        assert!(other.stakes.stake_delegations.len() >= 50);
        // DeserializableStakes doesn't preserve same order of elements as Stakes, compare converted
        let other_stakes = Stakes::from_deserialized(other.stakes);
        assert_eq!(other_stakes, dummy.stakes.into());
    }
}
