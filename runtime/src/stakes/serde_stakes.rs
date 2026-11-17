use {
    super::{StakeAccount, Stakes},
    solana_pubkey::Pubkey,
    solana_stake_interface::state::Stake,
    solana_vote::vote_account::VoteAccounts,
    std::{collections::HashMap, sync::Arc},
};

/// Wrapper struct with custom serialization to support serializing
/// `Stakes<StakeAccount>` as `Stakes<Stake>` without doing an intermediate
/// clone of the stake data.
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
