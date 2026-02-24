use {
    crate::bank::{Bank, EpochInflationRewards},
    solana_account::AccountSharedData,
    solana_clock::{Epoch, Slot},
    solana_pubkey::Pubkey,
    solana_system_interface::program as system_program,
    std::sync::LazyLock,
    thiserror::Error,
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

/// The state stored in the off curve account used to store metadata for calculating and paying
/// voting rewards.
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct VoteRewardAccountState {
    /// The rewards (in lamports) that would be paid to a validator whose stake is equal to the
    /// capitalization and it voted in every slot in the epoch.  This is also the epoch inflation.
    epoch_validator_rewards_lamports: u64,
}

impl VoteRewardAccountState {
    /// Returns the deserialized [`Self`] from the accounts in the [`Bank`].
    fn new_from_bank(bank: &Bank) -> Self {
        match bank.get_account(&VOTE_REWARD_ACCOUNT_ADDR) {
            None => {
                // this can happen in the first epoch when the account has not been created yet.
                // we create a dummy state to handle this case with the assumption that this code
                // will become active in an epoch before the epoch in which Alpenglow is activated.
                let state = Self {
                    epoch_validator_rewards_lamports: 0,
                };
                state.set_state(bank);
                state
            }
            Some(acct) => {
                // unwrap should be safe as the data being deserialized was serialized by us in
                // [`Self::set_state`].
                acct.deserialize_data().unwrap()
            }
        }
    }

    /// Serializes and updates [`Self`] into the accounts in the [`Bank`].
    fn set_state(&self, bank: &Bank) {
        // TODO: use wincode instead.
        let account_size = bincode::serialized_size(&self).unwrap();
        let lamports = bank
            .rent_collector()
            .rent
            .minimum_balance(account_size as usize);
        let account = AccountSharedData::new_data(lamports, &self, &system_program::ID).unwrap();
        bank.store_account_and_update_capitalization(&VOTE_REWARD_ACCOUNT_ADDR, &account);
    }

    /// Calculates and serializes a new version of [`Self`] into the accounts in the [`Bank`]
    /// when  a new epoch starts.   At the start of a new epoch, over several slots we pay the
    /// inflation rewards from the  previous epoch.  This is called Partitioned Epoch Rewards
    /// (PER).  As such, the capitalization keeps increasing in the first slots of the epoch.
    /// Vote rewards are calculated as a function of the capitalization and we do not want
    /// voting in the initial slots  to earn less rewards than voting in the later rewards.  As
    /// such this function is called with [`additional_validator_rewards`] which should be the
    /// total rewards that will be paid by PER and we use the capitalization from the previous
    /// epoch plus this value to compute the vote rewards.
    pub(crate) fn new_epoch_update_account(
        bank: &Bank,
        prev_epoch: Epoch,
        prev_epoch_capitalization: u64,
        additional_validator_rewards: u64,
    ) {
        let EpochInflationRewards {
            validator_rewards_lamports,
            epoch_duration_in_years: _,
            validator_rate: _,
            foundation_rate: _,
        } = bank.calculate_epoch_inflation_rewards(
            prev_epoch_capitalization + additional_validator_rewards,
            prev_epoch,
        );
        let state = Self {
            epoch_validator_rewards_lamports: validator_rewards_lamports,
        };
        state.set_state(bank);
    }

    /// Returns the amount of lamports needed to store this account.
    #[cfg(test)]
    pub(crate) fn rent_needed_for_account(bank: &Bank) -> u64 {
        let state = Self {
            epoch_validator_rewards_lamports: 0,
        };
        let account_size = bincode::serialized_size(&state).unwrap();
        bank.rent_collector()
            .rent
            .minimum_balance(account_size as usize)
    }
}

/// Different types of errors that can happen when calculating and paying voting reward.
#[derive(Debug, PartialEq, Eq, Error)]
pub enum PayVoteRewardError {
    #[error("missing epoch stakes")]
    MissingEpochStakes,
    #[error("missing validator")]
    MissingValidator,
}

/// Calculates and pays voting reward.
///
/// This is a NOP if [`reward_slot_and_validators`] is [`None`].
///
/// TODO: currently this function is just calculating rewards and not actually paying them.
pub(super) fn calculate_and_pay_voting_reward(
    bank: &Bank,
    reward_slot_and_validators: Option<(Slot, Vec<Pubkey>)>,
) -> Result<(), PayVoteRewardError> {
    let Some((reward_slot, validators)) = reward_slot_and_validators else {
        return Ok(());
    };

    let (vote_accounts, total_stake_lamports) = {
        let epoch_stakes = bank
            .epoch_stakes_from_slot(reward_slot)
            .ok_or(PayVoteRewardError::MissingEpochStakes)?;
        (
            epoch_stakes.stakes().vote_accounts().as_ref(),
            epoch_stakes.total_stake(),
        )
    };
    let epoch_validator_rewards_lamports =
        VoteRewardAccountState::new_from_bank(bank).epoch_validator_rewards_lamports;

    let mut total_leader_reward_lamports = 0u64;
    for validator in validators {
        let (validator_stake_lamports, _) = vote_accounts
            .get(&validator)
            .ok_or(PayVoteRewardError::MissingValidator)?;
        let reward_lamports = calculate_voting_reward(
            bank.epoch_schedule().slots_per_epoch,
            epoch_validator_rewards_lamports,
            total_stake_lamports,
            *validator_stake_lamports,
        );
        // As per the Alpenglow SIMD, the rewards are split equally between the validators and the leader.
        let validator_reward_lamports = reward_lamports / 2;
        let leader_reward_lamports = reward_lamports - validator_reward_lamports;
        total_leader_reward_lamports =
            total_leader_reward_lamports.saturating_add(leader_reward_lamports);
    }
    Ok(())
}

/// Computes the voting reward in Lamports.
fn calculate_voting_reward(
    slots_per_epoch: u64,
    epoch_validator_rewards_lamports: u64,
    total_stake_lamports: u64,
    validator_stake_lamports: u64,
) -> u64 {
    // Rewards are computed as following:
    // per_slot_inflation = epoch_validator_rewards_lamports / slots_per_epoch
    // fractional_stake = validator_stake / total_stake_lamports
    // rewards = fractional_stake * per_slot_inflation
    //
    // The code below is equivalent but changes the order of operations to maintain precision

    let numerator = epoch_validator_rewards_lamports as u128 * validator_stake_lamports as u128;
    let denominator = slots_per_epoch as u128 * total_stake_lamports as u128;

    // SAFETY: the result should fit in u64 because we do not expect the inflation in a single
    // epoch to exceed u64::MAX.
    (numerator / denominator).try_into().unwrap()
}

#[cfg(test)]
mod tests {
    use {super::*, solana_genesis_config::GenesisConfig, solana_native_token::LAMPORTS_PER_SOL};

    #[test]
    fn calculate_voting_reward_does_not_panic() {
        // the current circulating supply is about 566M.  The most extreme numbers are when all of
        // it is staked by a single validator.
        let circulating_supply = 566_000_000 * LAMPORTS_PER_SOL;

        let bank = Bank::new_for_tests(&GenesisConfig::default());
        let EpochInflationRewards {
            validator_rewards_lamports,
            ..
        } = bank.calculate_epoch_inflation_rewards(circulating_supply, 1);

        calculate_voting_reward(
            bank.epoch_schedule().slots_per_epoch,
            validator_rewards_lamports,
            circulating_supply,
            circulating_supply,
        );
    }

    #[test]
    fn serialization_works() {
        let bank = Bank::new_for_tests(&GenesisConfig::default());
        let state = VoteRewardAccountState {
            epoch_validator_rewards_lamports: 1234,
        };
        state.set_state(&bank);
        let deserialized = VoteRewardAccountState::new_from_bank(&bank);
        assert_eq!(state, deserialized);
    }

    #[test]
    fn epoch_update_works() {
        let bank = Bank::new_for_tests(&GenesisConfig::default());
        let prev_epoch = 1;
        let prev_epoch_capitalization = 12345;
        let additional_validator_rewards = 6789;
        VoteRewardAccountState::new_epoch_update_account(
            &bank,
            prev_epoch,
            prev_epoch_capitalization,
            additional_validator_rewards,
        );
        let VoteRewardAccountState {
            epoch_validator_rewards_lamports,
        } = VoteRewardAccountState::new_from_bank(&bank);
        let EpochInflationRewards {
            validator_rewards_lamports,
            ..
        } = bank.calculate_epoch_inflation_rewards(
            prev_epoch_capitalization + additional_validator_rewards,
            prev_epoch,
        );
        assert_eq!(epoch_validator_rewards_lamports, validator_rewards_lamports);
    }
}
