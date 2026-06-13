use {
    crate::bank::{Bank, MAX_ALPENGLOW_VOTE_ACCOUNTS},
    solana_account::{AccountSharedData, ReadableAccount},
    solana_clock::{Epoch, Slot},
    solana_pubkey::Pubkey,
    solana_sdk_ids::system_program,
    solana_vote::vote_account::VoteAccounts,
    std::{cmp::Ordering, collections::HashMap, sync::LazyLock},
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
    // This will be used in a follow up as part of non Tower PER calculations
    pub(crate) delegated_stakes: HashMap<Pubkey, u64>,
}

#[derive(Debug, SchemaRead, SchemaWrite)]
struct RewardEpochDelegatedStakesAccount {
    pub(crate) epoch: Epoch,
    pub(crate) delegated_stakes: Vec<RewardEpochDelegatedStake>,
}

#[derive(Debug, Clone, SchemaRead, SchemaWrite)]
struct RewardEpochDelegatedStake {
    pub(crate) vote_pubkey: Pubkey,
    pub(crate) delegated_stake: u64,
}

/// The off-curve account where we store the bounded reward-epoch delegated stake
/// denominators used for non-Tower reward recalculation after snapshot restore.
static REWARD_EPOCH_DELEGATED_STAKES_ACCOUNT: LazyLock<Pubkey> = LazyLock::new(|| {
    let (pubkey, _) = Pubkey::find_program_address(
        &[b"reward_epoch_delegated_stakes"],
        &agave_feature_set::alpenglow::id(),
    );
    pubkey
});

impl RewardEpochDelegatedStakesAccount {
    pub(crate) fn max_size() -> usize {
        static REWARD_EPOCH_DELEGATED_STAKES_ACCOUNT_MAX_SIZE: LazyLock<usize> =
            LazyLock::new(|| {
                let account = RewardEpochDelegatedStakesAccount {
                    epoch: u64::MAX,
                    delegated_stakes: vec![
                        RewardEpochDelegatedStake {
                            vote_pubkey: Pubkey::new_from_array([u8::MAX; 32]),
                            delegated_stake: u64::MAX,
                        };
                        MAX_ALPENGLOW_VOTE_ACCOUNTS
                    ],
                };
                wincode::serialized_size(&account)
                    .expect("reward epoch delegated stakes account must serialize")
                    .try_into()
                    .expect(
                        "reward epoch delegated stakes account serialized size must fit in usize",
                    )
            });

        *REWARD_EPOCH_DELEGATED_STAKES_ACCOUNT_MAX_SIZE
    }
}

impl RewardEpochDelegatedStakes {
    pub(crate) fn set(&self, bank: &Bank, distribution_vote_accounts: &VoteAccounts) {
        assert!(
            distribution_vote_accounts.len() <= MAX_ALPENGLOW_VOTE_ACCOUNTS,
            "reward epoch delegated stakes account must be bounded by MAX_ALPENGLOW_VOTE_ACCOUNTS"
        );

        let mut delegated_stakes = distribution_vote_accounts
            .delegated_stakes()
            .map(
                |(vote_pubkey, _delegated_stake)| RewardEpochDelegatedStake {
                    vote_pubkey: *vote_pubkey,
                    delegated_stake: self
                        .delegated_stakes
                        .get(vote_pubkey)
                        .copied()
                        .unwrap_or_default(),
                },
            )
            .collect::<Vec<_>>();
        delegated_stakes.sort_unstable_by_key(|stake| stake.vote_pubkey);

        let account = RewardEpochDelegatedStakesAccount {
            epoch: self.epoch,
            delegated_stakes,
        };
        let data = wincode::serialize(&account).unwrap();
        let lamports = bank
            .get_minimum_balance_for_rent_exemption(RewardEpochDelegatedStakesAccount::max_size());
        let mut account = AccountSharedData::new(lamports, data.len(), &system_program::ID);
        account.set_data_from_slice(&data);

        bank.store_account_and_update_capitalization(
            &REWARD_EPOCH_DELEGATED_STAKES_ACCOUNT,
            &account,
        );
    }

    pub(crate) fn get(bank: &Bank) -> Option<Self> {
        let account = bank.get_account(&REWARD_EPOCH_DELEGATED_STAKES_ACCOUNT)?;
        (!account.data().is_empty()).then(|| {
            let account: RewardEpochDelegatedStakesAccount = wincode::deserialize(account.data())
                .expect("Couldn't deserialize reward epoch delegated stakes");
            assert!(
                account.delegated_stakes.len() <= MAX_ALPENGLOW_VOTE_ACCOUNTS,
                "reward epoch delegated stakes account exceeds MAX_ALPENGLOW_VOTE_ACCOUNTS"
            );
            account.into()
        })
    }
}

impl From<RewardEpochDelegatedStakesAccount> for RewardEpochDelegatedStakes {
    fn from(account: RewardEpochDelegatedStakesAccount) -> Self {
        Self {
            epoch: account.epoch,
            delegated_stakes: account
                .delegated_stakes
                .into_iter()
                .map(|stake| (stake.vote_pubkey, stake.delegated_stake))
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
        // This will be used in a follow up as part of PER calculations for the migration epoch
        reward_epoch_delegated_stakes: RewardEpochDelegatedStakes,
    },
    /// This is a full alpenglow epoch
    Alpenglow {
        migration_epoch: Epoch,
        #[allow(dead_code)]
        // This will be used in a follow up as part of PER calculations for Alpenglow epochs
        reward_epoch_delegated_stakes: RewardEpochDelegatedStakes,
    },
}

impl AlpenglowEpochType {
    pub(crate) fn is_alpenglow_or_migration_epoch(bank: &Bank, epoch: Epoch) -> bool {
        debug_assert!(epoch < bank.epoch());
        bank.get_alpenglow_migration_slot()
            .map(|migration_slot| bank.epoch_schedule().get_epoch(migration_slot) <= epoch)
            .unwrap_or(false)
    }

    /// Returns `AlpenglowEpochType` for the given `epoch`.
    ///
    /// Calling this function with an epoch >= `Bank::epoch` can return false information as it is
    /// possible that we have not observed the genesis cert yet but will in the upcoming slots.
    ///
    /// We pass `reward_epoch_delegated_stakes` as a closure to avoid the potential deserialization
    /// of `REWARD_EPOCH_DELEGATED_STAKES_ACCOUNT` in the Tower case.
    pub(crate) fn get(
        bank: &Bank,
        epoch: Epoch,
        reward_epoch_delegated_stakes: impl FnOnce() -> Option<RewardEpochDelegatedStakes>,
    ) -> Self {
        debug_assert!(epoch < bank.epoch());
        let Some(migration_slot) = bank.get_alpenglow_migration_slot() else {
            return Self::Tower;
        };
        let migration_epoch = bank.epoch_schedule().get_epoch(migration_slot);
        match migration_epoch.cmp(&epoch) {
            Ordering::Less => {
                let reward_epoch_delegated_stakes =
                    reward_epoch_delegated_stakes().unwrap_or_else(|| {
                        panic!(
                            "Missing reward epoch delegated stakes for non-Tower reward epoch \
                             {epoch}"
                        )
                    });
                assert_eq!(
                    reward_epoch_delegated_stakes.epoch, epoch,
                    "reward epoch delegated stakes must match rewarded epoch"
                );
                Self::Alpenglow {
                    migration_epoch,
                    reward_epoch_delegated_stakes,
                }
            }
            Ordering::Greater => Self::Tower,
            Ordering::Equal => {
                let first_slot_in_epoch = bank
                    .epoch_schedule()
                    .get_first_slot_in_epoch(migration_epoch);
                // + 1 because the migration_slot is still tower.
                let num_tower_slots = migration_slot - first_slot_in_epoch + 1;
                let slots_in_epoch = bank.epoch_schedule().get_slots_in_epoch(migration_epoch);
                let num_ag_slots = slots_in_epoch - num_tower_slots;
                assert_eq!(slots_in_epoch, num_tower_slots + num_ag_slots);
                let reward_epoch_delegated_stakes =
                    reward_epoch_delegated_stakes().unwrap_or_else(|| {
                        panic!(
                            "Missing reward epoch delegated stakes for non-Tower reward epoch \
                             {epoch}"
                        )
                    });
                assert_eq!(
                    reward_epoch_delegated_stakes.epoch, epoch,
                    "reward epoch delegated stakes must match rewarded epoch"
                );
                Self::MigrationEpoch {
                    num_tower_slots,
                    num_ag_slots,
                    migration_epoch,
                    reward_epoch_delegated_stakes,
                }
            }
        }
    }
}
