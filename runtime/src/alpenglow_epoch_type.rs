use {
    serde::{Deserialize, Serialize},
    solana_clock::{Epoch, Slot},
    solana_pubkey::Pubkey,
    std::collections::HashMap,
    wincode::{SchemaRead, SchemaWrite},
};

/// For alpenglow rewards we only reward lamports earned in the previous epoch.
/// For this prior epoch we need to know the delegated stake for each vote account.
/// Note that this is not the same as `epoch_stakes`, which is calculated an epoch
/// in advance.
#[derive(Debug)]
pub(crate) struct RewardEpochDelegatedStakes {
    pub(crate) epoch: Epoch,
    #[allow(dead_code)]
    pub(crate) delegated_stakes: HashMap<Pubkey, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite)]
pub(crate) struct RewardEpochDelegatedStakesAccount {
    pub(crate) epoch: Epoch,
    pub(crate) delegated_stakes: Vec<RewardEpochDelegatedStake>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite)]
pub(crate) struct RewardEpochDelegatedStake {
    pub(crate) vote_pubkey: [u8; 32],
    pub(crate) delegated_stake: u64,
}

impl From<RewardEpochDelegatedStakesAccount> for RewardEpochDelegatedStakes {
    fn from(account: RewardEpochDelegatedStakesAccount) -> Self {
        Self {
            epoch: account.epoch,
            delegated_stakes: account
                .delegated_stakes
                .into_iter()
                .map(|stake| {
                    (
                        Pubkey::new_from_array(stake.vote_pubkey),
                        stake.delegated_stake,
                    )
                })
                .collect(),
        }
    }
}

#[derive(Debug)]
pub(crate) enum AlpenglowEpochType {
    /// This is a full tower epoch.
    Tower,
    /// The epoch started in tower and then switched to alpenglow
    MigrationEpoch {
        num_tower_slots: Slot,
        num_ag_slots: Slot,
        migration_epoch: Epoch,
        #[allow(dead_code)]
        reward_epoch_delegated_stakes: RewardEpochDelegatedStakes,
    },
    /// This is a full alpenglow epoch
    Alpenglow {
        migration_epoch: Epoch,
        #[allow(dead_code)]
        reward_epoch_delegated_stakes: RewardEpochDelegatedStakes,
    },
}
