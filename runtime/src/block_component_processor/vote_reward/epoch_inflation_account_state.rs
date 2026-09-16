use {
    crate::bank::Bank,
    serde::{Deserialize, Serialize},
    solana_account::{Account, AccountSharedData, ReadableAccount},
    solana_clock::Epoch,
    solana_genesis_config::GenesisConfig,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_system_interface::program as system_program,
    std::sync::LazyLock,
    wincode::{SchemaRead, SchemaWrite},
};

/// The account address for the off curve account used to store metadata for calculating and
/// paying voting rewards.
static VOTE_REWARD_ACCOUNT_ADDR: LazyLock<Pubkey> = LazyLock::new(|| {
    let (pubkey, _) = Pubkey::find_program_address(
        &[b"vote_reward_account"],
        &agave_feature_set::alpenglow::id(),
    );
    pubkey
});

#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample, StableAbi, StableAbiSample),
    frozen_abi(
        digest = "DwwQZJF7Epufk6MN9W6bfJ1z1DkjGfMZUsXDMWNv86jb",
        abi_digest = "CrSvqX8ZAYxjZ6XoTp9Z1ED6McdnqhXq8zYFWBDkCwJs",
        abi_serializer = "wincode",
        test_roundtrip = "eq_and_wire",
    )
)]
/// The per epoch info stored in the off curve account.
#[derive(Debug, Clone, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
pub(super) struct EpochInflationState {
    /// The rewards (in lamports) that would be paid to a validator whose stake is equal to the
    /// capitalization and it voted in every slot in the epoch.  This is also the
    /// epoch inflation.
    pub(super) max_possible_validator_reward: u64,
    /// Number of slots in the epoch.
    pub(super) slots_per_epoch: u64,
    /// The epoch number for this state.
    pub(super) epoch: Epoch,
}

impl EpochInflationState {
    fn new_from_bank(
        bank: &Bank,
        epoch_start_capitalization: u64,
        additional_rewards: u64,
    ) -> Self {
        let max_possible_validator_reward = bank.calculate_epoch_inflation_rewards(
            epoch_start_capitalization + additional_rewards,
            bank.epoch(),
        );
        EpochInflationState {
            max_possible_validator_reward,
            slots_per_epoch: bank.epoch_schedule.slots_per_epoch,
            epoch: bank.epoch(),
        }
    }
}

#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample, StableAbi, StableAbiSample),
    frozen_abi(
        digest = "HR1JbQp4gVU7fcsG4ji1fe28j8uNJRmuptdQz4PDkKoC",
        abi_digest = "FeEFnXTk7DxHkCamcHDpRRjRDfSyMh3DGnefbwSvA8Kc",
        abi_serializer = "wincode",
        test_roundtrip = "eq_and_wire",
    )
)]
/// The state stored in the off curve account used to store metadata for calculating and paying
/// voting rewards.
///
/// Info for the previous and the current epoch is stored.
#[derive(Debug, PartialEq, Eq, SchemaWrite, SchemaRead, Serialize, Deserialize)]
pub struct EpochInflationAccountState {
    /// [`EpochInflationState`] for the current epoch.
    current: EpochInflationState,
    /// [`EpochInflationState`] for the previous epoch.  This is needed for scenarios when the
    /// reward slot is in a previous epoch relative to the current slot.
    ///
    /// Stored in an `Option` because in the first epoch when Alpenglow is enabled, we will not
    /// have any state for the previous epoch.
    prev: Option<EpochInflationState>,
}

impl EpochInflationAccountState {
    /// Returns the deserialized [`Self`] from the accounts in the [`Bank`].
    ///
    /// Returns `None` if the `bank` does not contain the account or the account state is not of
    /// the expected size.
    pub(crate) fn new_from_bank(bank: &Bank) -> Option<Self> {
        bank.get_account(&VOTE_REWARD_ACCOUNT_ADDR)
            .and_then(|acct| wincode::deserialize(acct.data()).ok())
    }

    /// Returns the epoch-start inflation rewards recorded for `epoch`.
    pub(crate) fn inflation_rewards_for_epoch(self, epoch: Epoch) -> Option<u64> {
        self.get_epoch_state(epoch)
            .map(|state| state.max_possible_validator_reward)
    }

    /// Serializes and updates [`Self`] into the accounts in the [`Bank`].
    fn set_state(&self, bank: &Bank) {
        let data = wincode::serialize(&self).unwrap();
        let lamports = bank.get_minimum_balance_for_rent_exemption(data.len());
        let mut account = AccountSharedData::new(lamports, data.len(), &system_program::ID);
        account.set_data_from_slice(&data);
        bank.store_account_and_update_capitalization(&VOTE_REWARD_ACCOUNT_ADDR, &account);
    }

    /// Inserts a dummy account for [`Self`] into the `genesis_config` to aid local cluster testing.
    pub(crate) fn insert_into_genesis_config(genesis_config: &mut GenesisConfig) {
        let current = EpochInflationState {
            max_possible_validator_reward: 0,
            slots_per_epoch: genesis_config.epoch_schedule.slots_per_epoch,
            epoch: 0,
        };
        let account = Self {
            current,
            prev: None,
        };
        let account_size = wincode::serialized_size(&account).unwrap();
        let lamports = Rent::default()
            .minimum_balance(account_size.try_into().unwrap())
            .max(1);
        let account = Account::new_data(lamports, &account, &system_program::ID).unwrap();
        genesis_config
            .accounts
            .insert(*VOTE_REWARD_ACCOUNT_ADDR, account);
    }

    /// Computes a new version of `Self` for `bank.epoch` and serializes it into accounts in the `bank`.
    ///
    /// At the start of a new epoch, over several slots we pay the inflation rewards from the
    /// previous epoch.  This is called Partitioned Epoch Rewards (PER).  As such, the
    /// capitalization keeps increasing in the first slots of the epoch.  Vote rewards are
    /// calculated as a function of the capitalization and we do not want voting in the initial
    /// slots to earn less rewards than voting in the later rewards.  As such this function is
    /// called with [`additional_rewards`] which should be the total rewards that will
    /// be paid by PER and we use the capitalization from the previous epoch plus this value to
    /// compute the vote rewards.
    pub(crate) fn new_epoch_update_account(
        bank: &Bank,
        epoch_start_capitalization: u64,
        additional_rewards: u64,
    ) {
        let prev = Self::new_from_bank(bank).map(|s| s.current);
        let current = EpochInflationState::new_from_bank(
            bank,
            epoch_start_capitalization,
            additional_rewards,
        );
        let state = Self { prev, current };
        state.set_state(bank);
    }

    /// Returns the [`EpochState`] corresponding to the given `epoch`.
    pub(super) fn get_epoch_state(self, epoch: Epoch) -> Option<EpochInflationState> {
        if self.current.epoch == epoch {
            return Some(self.current);
        }
        if let Some(prev) = self.prev
            && prev.epoch == epoch
        {
            return Some(prev);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            bank_forks::BankForks,
            genesis_utils::{
                GenesisConfigInfo, ValidatorVoteKeypairs, create_genesis_config,
                create_genesis_config_with_alpenglow_vote_accounts, deactivate_features,
            },
            slot_params::slot_time_feature_ids,
        },
        agave_feature_set as feature_set,
        rand::Rng,
        solana_epoch_schedule::EpochSchedule,
        solana_genesis_config::GenesisConfig,
        solana_inflation::Inflation,
        solana_leader_schedule::SlotLeader,
        std::sync::Arc,
    };

    fn get_rand_state() -> EpochInflationAccountState {
        let mut rng = rand::rng();
        EpochInflationAccountState {
            prev: None,
            current: EpochInflationState {
                max_possible_validator_reward: rng.random(),
                slots_per_epoch: rng.random(),
                epoch: rng.random(),
            },
        }
    }

    #[test]
    fn set_state_works() {
        let bank_forks = BankForks::new_rw_arc(Bank::new_for_tests(&GenesisConfig::default()));
        let bank = bank_forks.read().unwrap().root_bank();
        let state = get_rand_state();
        state.set_state(&bank);
        let deserialized = EpochInflationAccountState::new_from_bank(&bank).unwrap();
        assert_eq!(state, deserialized);
    }

    #[test]
    fn new_epoch_update_account_works() {
        let (bank_epoch_0, bank_epoch_1, bank_epoch_2) = {
            let validator_keypairs = (0..10)
                .map(|_| ValidatorVoteKeypairs::new_rand())
                .collect::<Vec<_>>();
            let genesis = create_genesis_config_with_alpenglow_vote_accounts(
                1_000_000_000,
                &validator_keypairs,
                vec![100; validator_keypairs.len()],
            );
            let bank_forks = BankForks::new_rw_arc(Bank::new_for_tests(&genesis.genesis_config));
            let bank_epoch_0 = bank_forks.read().unwrap().root_bank();
            let first_slot_in_epoch_1 = bank_epoch_0.epoch_schedule().get_first_slot_in_epoch(1);
            let bank_epoch_1 = Arc::new(Bank::new_from_parent(
                bank_epoch_0.clone(),
                SlotLeader::new_unique(),
                first_slot_in_epoch_1,
            ));
            assert_eq!(bank_epoch_1.epoch(), 1);
            let first_slot_in_epoch_2 = bank_epoch_1.epoch_schedule().get_first_slot_in_epoch(2);
            let bank_epoch_2 = Arc::new(Bank::new_from_parent(
                bank_epoch_1.clone(),
                SlotLeader::new_unique(),
                first_slot_in_epoch_2,
            ));
            (bank_epoch_0, bank_epoch_1, bank_epoch_2)
        };
        assert_eq!(bank_epoch_0.epoch(), 0);
        assert_eq!(bank_epoch_1.epoch(), 1);
        assert_eq!(bank_epoch_2.epoch(), 2);

        let expected_prev =
            EpochInflationState::new_from_bank(&bank_epoch_1, bank_epoch_0.capitalization(), 0);
        let expected_current =
            EpochInflationState::new_from_bank(&bank_epoch_2, bank_epoch_1.capitalization(), 0);
        let EpochInflationAccountState { current, prev } =
            EpochInflationAccountState::new_from_bank(&bank_epoch_2).unwrap();
        assert_eq!(current, expected_current);
        assert_eq!(prev.unwrap(), expected_prev);
    }

    #[test]
    fn new_epoch_update_account_uses_current_epoch_rewards() {
        let GenesisConfigInfo {
            mut genesis_config, ..
        } = create_genesis_config(1_000_000_000);
        genesis_config.inflation = Inflation::full();
        // Warmup is necessary here to give epoch 0 and epoch 1 different reward
        // budgets.
        genesis_config.epoch_schedule = EpochSchedule::custom(64, 64, true);
        deactivate_features(&mut genesis_config, &slot_time_feature_ids().to_vec());

        for (case, genesis_config, activate_slot_time_feature) in [
            ("current reward budget", genesis_config, false),
            (
                "current epoch slot params",
                GenesisConfig {
                    inflation: Inflation::full(),
                    ..GenesisConfig::default()
                },
                true,
            ),
        ] {
            let mut bank_epoch_0 = Bank::new_for_tests(&genesis_config);
            if activate_slot_time_feature {
                bank_epoch_0.activate_feature(&feature_set::reduce_slot_time_to_350ms::id());
            }
            let bank_forks = BankForks::new_rw_arc(bank_epoch_0);
            let bank_epoch_0 = bank_forks.read().unwrap().root_bank();
            let epoch_1_first_slot = bank_epoch_0.epoch_schedule().get_first_slot_in_epoch(1);
            let bank_epoch_1 = Bank::new_from_parent_with_bank_forks(
                bank_forks.as_ref(),
                bank_epoch_0.clone(),
                SlotLeader::new_unique(),
                epoch_1_first_slot,
            );

            assert_eq!(bank_epoch_0.epoch(), 0, "{case}");
            assert_eq!(bank_epoch_1.epoch(), 1, "{case}");
            if activate_slot_time_feature {
                assert_ne!(bank_epoch_0.ns_per_slot, bank_epoch_1.ns_per_slot, "{case}");
            } else {
                assert_ne!(
                    bank_epoch_1.epoch_schedule().get_slots_in_epoch(0),
                    bank_epoch_1.epoch_schedule().get_slots_in_epoch(1),
                    "{case}",
                );
            }

            let epoch_start_capitalization = bank_epoch_0.capitalization();
            let expected_current_epoch_rewards = bank_epoch_1.calculate_epoch_inflation_rewards(
                epoch_start_capitalization,
                bank_epoch_1.epoch(),
            );
            assert_ne!(
                expected_current_epoch_rewards,
                bank_epoch_1.calculate_epoch_inflation_rewards(
                    epoch_start_capitalization,
                    bank_epoch_0.epoch()
                ),
                "{case}",
            );

            EpochInflationAccountState::new_epoch_update_account(
                &bank_epoch_1,
                epoch_start_capitalization,
                0,
            );
            let state = EpochInflationAccountState::new_from_bank(&bank_epoch_1).unwrap();
            assert_eq!(state.current.epoch, bank_epoch_1.epoch(), "{case}");
            assert_eq!(
                state.current.max_possible_validator_reward, expected_current_epoch_rewards,
                "{case}",
            );
        }
    }

    #[test]
    fn handles_prefunded_account() {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(10_000);
        let bank_forks = BankForks::new_rw_arc(Bank::new_for_tests(&genesis_config));
        let root_bank = bank_forks.read().unwrap().root_bank();

        let prefund_lamports = 100;
        root_bank
            .transfer(prefund_lamports, &mint_keypair, &VOTE_REWARD_ACCOUNT_ADDR)
            .unwrap();

        assert!(root_bank.get_account(&VOTE_REWARD_ACCOUNT_ADDR).is_some());
        assert_eq!(
            root_bank.get_balance(&VOTE_REWARD_ACCOUNT_ADDR),
            prefund_lamports,
        );
        assert_eq!(EpochInflationAccountState::new_from_bank(&root_bank), None);
    }
}
