use {
    crate::{bank::Bank, validated_reward_certificate::ValidatedRewardCert},
    bincode::Error as BincodeError,
    epoch_inflation_account_state::{EpochInflationAccountState, EpochInflationState},
    log::info,
    solana_account::{AccountSharedData, ReadableAccount, WritableAccount},
    solana_clock::{Epoch, Slot},
    solana_pubkey::Pubkey,
    solana_transaction::InstructionError,
    solana_vote::vote_account::VoteAccount,
    solana_vote_interface::state::{LandedVote, Lockout},
    solana_vote_program::vote_state::handler::VoteStateHandler,
    std::collections::{HashMap, HashSet, VecDeque},
    thiserror::Error,
};

pub mod epoch_inflation_account_state;

/// Different types of errors that can happen when calculating and paying voting reward.
#[derive(Debug, Error)]
pub(super) enum VoteRewardError {
    #[error("Missing rank map for reward cert or final cert input")]
    RankMapNotFound,
    #[error("Creating reward state failed with {0}")]
    StateNewReward(StateError),
    #[error("Creating both state failed with {0}")]
    StateNewBoth(StateError),
}

#[derive(Debug, Error)]
pub(super) enum VoteStateError {
    #[error("could not find the vote account")]
    AccountNotFound,
    #[error("deserializing vote account failed with {0}")]
    DeserializeFailed(BincodeError),
    #[error("converting account into VoteStateHandler failed with {0}")]
    HandlerConversionFailed(InstructionError),
    #[error("serializing VoteStateHandler failed with {0}")]
    SerializeFailed(InstructionError),
}

/// Data needed to operate on `VoteStateHandler`.
struct VoteState {
    /// Reference to actual `VoteStateHandler`.
    handler: VoteStateHandler,
    /// How many lamports were stored in the account.
    lamports: u64,
    /// How much space the account takes up.
    space: usize,
    /// Who owns the account.
    owner: Pubkey,
}

impl VoteState {
    fn try_new(
        vote_accounts: &HashMap<Pubkey, (u64, VoteAccount)>,
        vote_pubkey: Pubkey,
    ) -> Result<Self, VoteStateError> {
        let account = vote_accounts
            .get(&vote_pubkey)
            .ok_or(VoteStateError::AccountNotFound)?
            .1
            .account();
        let versions =
            bincode::deserialize(account.data()).map_err(VoteStateError::DeserializeFailed)?;
        let handler = VoteStateHandler::try_new_from_vote_state_versions(versions)
            .map_err(VoteStateError::HandlerConversionFailed)?;
        Ok(Self {
            handler,
            lamports: account.lamports(),
            space: account.data().len(),
            owner: *account.owner(),
        })
    }

    fn serialize(self) -> Result<AccountSharedData, VoteStateError> {
        let mut updated_account = AccountSharedData::new(self.lamports, self.space, &self.owner);
        self.handler
            .serialize_into(updated_account.data_as_mut_slice())
            .map_err(VoteStateError::SerializeFailed)?;
        Ok(updated_account)
    }
}

#[derive(Debug, PartialEq, Eq, Error)]
pub(super) enum RewardStateError {
    #[error("missing epoch stakes for reward_slot {reward_slot} in current_slot {current_slot}")]
    MissingEpochStakes {
        reward_slot: Slot,
        current_slot: Slot,
    },
    #[error("missing EpochInflationAccountState for current slot {current_slot}")]
    MissingEpochInflationAccountState { current_slot: Slot },
    #[error(
        "missing validator stake info for reward epoch {reward_epoch} in current_slot \
         {current_slot}"
    )]
    NoEpochValidatorStake {
        reward_epoch: Epoch,
        current_slot: Slot,
    },
    #[error(
        "validator {pubkey} missing in current slot {current_slot} for reward slot {reward_slot}"
    )]
    MissingRewardSlotValidator {
        pubkey: Pubkey,
        reward_slot: Slot,
        current_slot: Slot,
    },
}

/// Common state required to pay rewards.
#[derive(Debug)]
struct RewardState<'a> {
    /// The epoch in which the reward was paid into the vote account.
    current_epoch: Epoch,
    /// The slot in which the reward was earned.
    reward_slot: Slot,
    /// Validators present in the reward cert.
    reward_validators: &'a HashSet<Pubkey>,
    /// The slot in which the reward is being paid into the vote account.
    current_slot: Slot,
    /// The pubkey of the validator that will receive the leader rewards.
    leader_vote_pubkey: Pubkey,
    /// Vote accounts at reward slot.
    accounts: &'a HashMap<Pubkey, (u64, VoteAccount)>,
    /// Total stake at `reward_slot`.
    total_stake: u64,
    /// inflation state at `reward_slot`.
    epoch_inflation_state: EpochInflationState,
}

impl<'a> RewardState<'a> {
    fn new(
        bank: &'a Bank,
        reward_slot: Slot,
        reward_validators: &'a HashSet<Pubkey>,
    ) -> Result<Self, RewardStateError> {
        let current_slot = bank.slot();
        let epoch_stakes = bank.epoch_stakes_from_slot(reward_slot).ok_or(
            RewardStateError::MissingEpochStakes {
                reward_slot,
                current_slot,
            },
        )?;
        let accounts = epoch_stakes.stakes().vote_accounts().as_ref();
        let total_stake = epoch_stakes.total_stake();
        // This assumes that if the epoch_schedule ever changes, the new schedule will maintain correct
        // info about older slots as well.
        let reward_epoch = bank.epoch_schedule.get_epoch(reward_slot);
        let epoch_inflation_state = {
            let epoch_inflation_account_state = EpochInflationAccountState::new_from_bank(bank);
            // This function should only be called after alpenglow is active and the slot in the the epoch
            // that activated Alpenglow should have created the account.
            debug_assert!(epoch_inflation_account_state.is_some());
            epoch_inflation_account_state
                .ok_or(RewardStateError::MissingEpochInflationAccountState { current_slot })?
                .get_epoch_state(reward_epoch)
                .ok_or(RewardStateError::NoEpochValidatorStake {
                    reward_epoch,
                    current_slot,
                })?
        };
        Ok(Self {
            current_epoch: bank.epoch(),
            reward_slot,
            reward_validators,
            current_slot,
            leader_vote_pubkey: bank.leader().vote_address,
            accounts,
            total_stake,
            epoch_inflation_state,
        })
    }

    /// Calculates rewards for the `validator`.
    ///
    /// On success also increments `total_leader_reward` with the leader's share.
    fn calculate_reward(
        &self,
        validator: Pubkey,
        total_leader_reward: &mut u64,
    ) -> Result<u64, RewardStateError> {
        let (reward_slot_validator_stake, _) =
            self.accounts
                .get(&validator)
                .ok_or(RewardStateError::MissingRewardSlotValidator {
                    pubkey: validator,
                    reward_slot: self.reward_slot,
                    current_slot: self.current_slot,
                })?;
        let (validator_reward, leader_reward) = calculate_reward(
            &self.epoch_inflation_state,
            self.total_stake,
            *reward_slot_validator_stake,
        );
        *total_leader_reward = total_leader_reward.saturating_add(leader_reward);
        Ok(validator_reward)
    }

    /// Pays `reward` into `epoch_credits` field and updates the `votes` field in `VoteStateHandler`.
    ///
    /// Returns whether or not the account was actually updated.
    #[must_use]
    fn update_account(
        &self,
        vote_pubkey: Pubkey,
        reward: u64,
        new_root_slot: Slot,
        handler: &mut VoteStateHandler,
    ) -> bool {
        let mut updated = false;
        if reward != 0 {
            handler.increment_credits(self.current_epoch, reward);
            updated = true;
        }
        if self.reward_validators.contains(&vote_pubkey) {
            let latest_root = handler
                .root_slot()
                .unwrap_or(new_root_slot)
                .max(new_root_slot);
            handler.set_votes(VecDeque::from([LandedVote {
                lockout: Lockout::new(latest_root),
                latency: 0,
            }]));
            true
        } else {
            updated
        }
    }
}

/// Common state required to update root_slot on the vote account.
#[derive(Debug)]
struct FinalCertState<'a> {
    leader_vote_pubkey: Pubkey,
    signers: &'a HashSet<Pubkey>,
    final_slot: Slot,
}

impl<'a> FinalCertState<'a> {
    /// Updates the `root_slot` field in the `VoteStateHandler` if the finalization cert was signed
    /// by this validator.
    #[must_use]
    fn update_account(&self, vote_pubkey: Pubkey, handler: &mut VoteStateHandler) -> bool {
        if self.signers.contains(&vote_pubkey) {
            handler.set_root_slot(Some(self.final_slot));
            true
        } else {
            false
        }
    }
}

#[derive(Debug, Error)]
pub(super) enum StateError {
    #[error("RewardState::new() failed with {0}")]
    RewardStateNew(RewardStateError),
    #[error("RewardState::calculate_reward() failed with {0}")]
    RewardStateCalcReward(RewardStateError),
    #[error("VoteState::new() failed with {0}")]
    VoteStateNew(VoteStateError),
    #[error("VoteState::serialize() failed with {0}")]
    VoteStateSerialize(VoteStateError),
}

/// Different variants that can be returned from `State::update_account`.
enum UpdateAccountRes {
    /// Handled updates on a non-leader validator
    NonLeader(Option<AccountSharedData>),
    /// Attempted to update the leader validator.  Should be updated using the update_leader() method.
    Leader,
}

/// enum to handle whether we are handling rewards; finalization cert; or both.
#[derive(Debug)]
enum State<'a> {
    /// Handling just rewards.
    Reward(RewardState<'a>),
    /// Handling just finalization cert.
    FinalCert(FinalCertState<'a>),
    /// Handling both.
    Both(RewardState<'a>, FinalCertState<'a>),
}

impl<'a> State<'a> {
    fn new_reward(
        bank: &'a Bank,
        reward_slot: Slot,
        validators: &'a HashSet<Pubkey>,
    ) -> Result<Self, StateError> {
        Ok(Self::Reward(
            RewardState::new(bank, reward_slot, validators).map_err(StateError::RewardStateNew)?,
        ))
    }

    fn new_final_cert(bank: &'a Bank, signers: &'a HashSet<Pubkey>, final_slot: Slot) -> Self {
        Self::FinalCert(FinalCertState {
            leader_vote_pubkey: bank.leader().vote_address,
            signers,
            final_slot,
        })
    }

    fn new_both(
        bank: &'a Bank,
        reward_slot: Slot,
        reward_validators: &'a HashSet<Pubkey>,
        final_slot: Slot,
        signers: &'a HashSet<Pubkey>,
    ) -> Result<Self, StateError> {
        let reward_state = RewardState::new(bank, reward_slot, reward_validators)
            .map_err(StateError::RewardStateNew)?;
        let final_cert_state = FinalCertState {
            leader_vote_pubkey: bank.leader().vote_address,
            signers,
            final_slot,
        };
        Ok(Self::Both(reward_state, final_cert_state))
    }

    /// Depending on the variant of `Self`, looks up the vote account and updates it.
    fn update_account(
        &self,
        vote_accounts: &HashMap<Pubkey, (u64, VoteAccount)>,
        vote_pubkey: Pubkey,
        total_leader_reward: &mut u64,
    ) -> Result<UpdateAccountRes, StateError> {
        match self {
            Self::Reward(state) => {
                let validator_reward = state
                    .calculate_reward(vote_pubkey, total_leader_reward)
                    .map_err(StateError::RewardStateCalcReward)?;
                if vote_pubkey == state.leader_vote_pubkey {
                    *total_leader_reward = total_leader_reward.saturating_add(validator_reward);
                    return Ok(UpdateAccountRes::Leader);
                }
                let mut vote_state = VoteState::try_new(vote_accounts, vote_pubkey)
                    .map_err(StateError::VoteStateNew)?;
                let updated_account = state
                    .update_account(
                        vote_pubkey,
                        validator_reward,
                        state.reward_slot,
                        &mut vote_state.handler,
                    )
                    .then_some(
                        vote_state
                            .serialize()
                            .map_err(StateError::VoteStateSerialize)?,
                    );
                Ok(UpdateAccountRes::NonLeader(updated_account))
            }
            Self::FinalCert(state) => {
                if vote_pubkey == state.leader_vote_pubkey {
                    return Ok(UpdateAccountRes::Leader);
                }
                let mut vote_state = VoteState::try_new(vote_accounts, vote_pubkey)
                    .map_err(StateError::VoteStateNew)?;
                let updated_account = state
                    .update_account(vote_pubkey, &mut vote_state.handler)
                    .then_some(
                        vote_state
                            .serialize()
                            .map_err(StateError::VoteStateSerialize)?,
                    );
                Ok(UpdateAccountRes::NonLeader(updated_account))
            }
            Self::Both(reward_state, final_cert_state) => {
                let validator_reward = reward_state
                    .calculate_reward(vote_pubkey, total_leader_reward)
                    .map_err(StateError::RewardStateCalcReward)?;
                if vote_pubkey == reward_state.leader_vote_pubkey {
                    *total_leader_reward = total_leader_reward.saturating_add(validator_reward);
                    return Ok(UpdateAccountRes::Leader);
                }
                let mut vote_state = VoteState::try_new(vote_accounts, vote_pubkey)
                    .map_err(StateError::VoteStateNew)?;

                let new_root_slot = if final_cert_state.signers.contains(&vote_pubkey) {
                    reward_state.reward_slot.max(final_cert_state.final_slot)
                } else {
                    reward_state.reward_slot
                };
                let reward_updated = reward_state.update_account(
                    vote_pubkey,
                    validator_reward,
                    new_root_slot,
                    &mut vote_state.handler,
                );
                let final_cert_updated =
                    final_cert_state.update_account(vote_pubkey, &mut vote_state.handler);
                let updated_account = (reward_updated | final_cert_updated).then_some(
                    vote_state
                        .serialize()
                        .map_err(StateError::VoteStateSerialize)?,
                );
                Ok(UpdateAccountRes::NonLeader(updated_account))
            }
        }
    }

    /// Updates all the validators in the provided `validators` iterator.
    ///
    /// The updated validator accounts are stored in `updated_accounts`.
    /// `total_leader_reward` is incremented by the leader's share of the reward observed.
    fn update_accounts(
        &self,
        vote_accounts: &HashMap<Pubkey, (u64, VoteAccount)>,
        validators: impl Iterator<Item = &'a Pubkey>,
        updated_accounts: &mut Vec<(Pubkey, AccountSharedData)>,
        total_leader_reward: &mut u64,
    ) {
        for &validator in validators {
            match self.update_account(vote_accounts, validator, total_leader_reward) {
                Ok(UpdateAccountRes::NonLeader(Some(acct))) => {
                    updated_accounts.push((validator, acct))
                }
                Ok(UpdateAccountRes::Leader) | Ok(UpdateAccountRes::NonLeader(None)) => {}
                Err(e) => {
                    info!(
                        "State=\"{self:?}\": update_account(validator={validator}) failed with {e}"
                    );
                }
            }
        }
    }

    /// Once the total rewards for the leader have been calculated, this is used to update leader
    /// vote account state.
    fn update_leader(
        &self,
        vote_accounts: &HashMap<Pubkey, (u64, VoteAccount)>,
        leader_vote_pubkey: Pubkey,
        reward: u64,
        updated_accounts: &mut Vec<(Pubkey, AccountSharedData)>,
    ) -> Result<(), StateError> {
        let mut vote_state = VoteState::try_new(vote_accounts, leader_vote_pubkey)
            .map_err(StateError::VoteStateNew)?;
        let updated = match self {
            Self::FinalCert(state) => {
                debug_assert_eq!(leader_vote_pubkey, state.leader_vote_pubkey);
                state.update_account(leader_vote_pubkey, &mut vote_state.handler)
            }
            Self::Reward(state) => {
                debug_assert_eq!(leader_vote_pubkey, state.leader_vote_pubkey);
                state.update_account(
                    leader_vote_pubkey,
                    reward,
                    state.reward_slot,
                    &mut vote_state.handler,
                )
            }
            Self::Both(reward_state, final_cert_state) => {
                debug_assert_eq!(leader_vote_pubkey, reward_state.leader_vote_pubkey);
                debug_assert_eq!(
                    reward_state.leader_vote_pubkey,
                    final_cert_state.leader_vote_pubkey
                );
                let new_root_slot = if final_cert_state.signers.contains(&leader_vote_pubkey) {
                    final_cert_state.final_slot.max(reward_state.reward_slot)
                } else {
                    reward_state.reward_slot
                };
                let reward_updated = reward_state.update_account(
                    leader_vote_pubkey,
                    reward,
                    new_root_slot,
                    &mut vote_state.handler,
                );
                let final_cert_updated = final_cert_state
                    .update_account(final_cert_state.leader_vote_pubkey, &mut vote_state.handler);
                reward_updated || final_cert_updated
            }
        };
        if updated {
            let updated_account = vote_state
                .serialize()
                .map_err(StateError::VoteStateSerialize)?;
            updated_accounts.push((leader_vote_pubkey, updated_account));
        }
        Ok(())
    }
}

/// Allocates a `Vec` for storing updated accounts.  Uses `Vec::with_capacity` to reduce reallocations.
fn allocate_updated_accounts(
    bank: &Bank,
    reward_cert: &Option<ValidatedRewardCert>,
    final_cert_input: &Option<(&HashSet<Pubkey>, Slot)>,
) -> Result<Option<Vec<(Pubkey, AccountSharedData)>>, VoteRewardError> {
    let max_validators = match (&reward_cert, &final_cert_input) {
        (None, None) => return Ok(None),
        (Some(cert), None) => {
            // Adding one in the off chance that the current leader is not in the cert.
            cert.validators().len() + 1
        }
        (None, Some((signers, _))) => signers.len(),
        (Some(reward_cert), Some((_, slot))) => {
            // Both finalization cert and reward cert are present.  Instead of computing overlap,
            // use max validators.
            let final_cert_slot_max_validators = bank
                .get_rank_map(*slot)
                .ok_or(VoteRewardError::RankMapNotFound)?
                .len();
            let reward_cert_slot_max_validators = bank
                .get_rank_map(reward_cert.slot())
                .ok_or(VoteRewardError::RankMapNotFound)?
                .len();
            final_cert_slot_max_validators.max(reward_cert_slot_max_validators)
        }
    };
    Ok(Some(Vec::with_capacity(max_validators)))
}

/// Calculates voting rewards based on the `reward_cert` and updates fields in the vote account
/// based on the calculated rewards and the `final_cert_input`.
pub(super) fn calc_vote_rewards_update_vote_states(
    bank: &Bank,
    reward_cert: Option<ValidatedRewardCert>,
    final_cert_input: Option<(&HashSet<Pubkey>, Slot)>,
) -> Result<(), VoteRewardError> {
    let Some(mut updated_accounts) =
        allocate_updated_accounts(bank, &reward_cert, &final_cert_input)?
    else {
        return Ok(());
    };

    let vote_accounts = bank.vote_accounts();

    match (reward_cert, final_cert_input) {
        (None, None) => Ok(()),

        (None, Some((signers, final_slot))) => {
            let mut total_leader_reward = 0;
            let state = State::new_final_cert(bank, signers, final_slot);
            state.update_accounts(
                &vote_accounts,
                signers.iter(),
                &mut updated_accounts,
                &mut total_leader_reward,
            );
            assert_eq!(total_leader_reward, 0);
            let leader_vote_pubkey = bank.leader().vote_address;
            if let Err(e) = state.update_leader(
                &vote_accounts,
                leader_vote_pubkey,
                total_leader_reward,
                &mut updated_accounts,
            ) {
                info!(
                    "State=\"{state:?}\": update_leader(leader={leader_vote_pubkey}) failed with \
                     {e}"
                );
            }
            bank.store_accounts((bank.slot(), updated_accounts.as_slice()));
            Ok(())
        }

        (Some(reward_cert), None) => {
            let (reward_slot, reward_validators) = reward_cert.into_parts();
            let mut total_leader_reward = 0;
            let state = State::new_reward(bank, reward_slot, &reward_validators)
                .map_err(VoteRewardError::StateNewReward)?;
            state.update_accounts(
                &vote_accounts,
                reward_validators.iter(),
                &mut updated_accounts,
                &mut total_leader_reward,
            );
            let leader_vote_pubkey = bank.leader().vote_address;
            if let Err(e) = state.update_leader(
                &vote_accounts,
                leader_vote_pubkey,
                total_leader_reward,
                &mut updated_accounts,
            ) {
                info!(
                    "State=\"{state:?}\": update_leader(leader={leader_vote_pubkey}) failed with \
                     {e}"
                );
            }
            bank.store_accounts((bank.slot(), updated_accounts.as_slice()));
            Ok(())
        }

        (Some(reward_cert), Some((signers, final_slot))) => {
            // both finalization cert and reward cert are present.  We have to update three sets of
            // validators: those present in both; those just in reward cert; and those just in the
            // finalization cert.

            let (reward_slot, reward_validators) = reward_cert.into_parts();
            let mut total_leader_reward = 0;
            {
                let state = State::new_final_cert(bank, signers, final_slot);
                state.update_accounts(
                    &vote_accounts,
                    signers.difference(&reward_validators),
                    &mut updated_accounts,
                    &mut total_leader_reward,
                );
                assert_eq!(total_leader_reward, 0);
            }

            {
                let state = State::new_reward(bank, reward_slot, &reward_validators)
                    .map_err(VoteRewardError::StateNewReward)?;
                state.update_accounts(
                    &vote_accounts,
                    reward_validators.difference(signers),
                    &mut updated_accounts,
                    &mut total_leader_reward,
                );
            }

            {
                let state =
                    State::new_both(bank, reward_slot, &reward_validators, final_slot, signers)
                        .map_err(VoteRewardError::StateNewBoth)?;
                state.update_accounts(
                    &vote_accounts,
                    reward_validators.intersection(signers),
                    &mut updated_accounts,
                    &mut total_leader_reward,
                );

                // Now that all validators have been handled, we have computed the total leader
                // reward so can update it.
                let leader_vote_pubkey = bank.leader().vote_address;
                if let Err(e) = state.update_leader(
                    &vote_accounts,
                    leader_vote_pubkey,
                    total_leader_reward,
                    &mut updated_accounts,
                ) {
                    info!(
                        "State=\"{state:?}\": update_leader(leader={leader_vote_pubkey}) failed \
                         with {e}"
                    );
                }
            }
            bank.store_accounts((bank.slot(), updated_accounts.as_slice()));
            Ok(())
        }
    }
}

/// Computes the voting reward in Lamports.
///
/// Returns `(validator rewards, leader rewards)`.
fn calculate_reward(
    epoch_state: &EpochInflationState,
    total_stake_lamports: u64,
    validator_stake_lamports: u64,
) -> (u64, u64) {
    // Rewards are computed as following:
    // per_slot_inflation = epoch_validator_rewards_lamports / slots_per_epoch
    // fractional_stake = validator_stake / total_stake_lamports
    // rewards = fractional_stake * per_slot_inflation
    //
    // The code below is equivalent but changes the order of operations to maintain precision

    let numerator =
        epoch_state.max_possible_validator_reward as u128 * validator_stake_lamports as u128;
    let denominator = epoch_state.slots_per_epoch as u128 * total_stake_lamports as u128;

    // SAFETY: the result should fit in u64 because we do not expect the inflation in a single
    // epoch to exceed u64::MAX.
    let reward_lamports: u64 = (numerator / denominator).try_into().unwrap();
    // As per the Alpenglow SIMD, the rewards are split equally between the validators and the leader.
    let validator_reward_lamports = reward_lamports / 2;
    let leader_reward_lamports = reward_lamports - validator_reward_lamports;
    (validator_reward_lamports, leader_reward_lamports)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            bank::VAT_TO_BURN_PER_EPOCH,
            bank_forks::BankForks,
            genesis_utils::{
                ValidatorVoteKeypairs, activate_all_features_alpenglow,
                create_genesis_config_with_alpenglow_vote_accounts,
                create_genesis_config_with_leader_ex, create_validator,
            },
            inflation_rewards::commission_split,
            stake_utils,
            validated_block_finalization::ValidatedBlockFinalizationCert,
        },
        agave_feature_set::FeatureSet,
        agave_votor_messages::{
            consensus_message::{Certificate, CertificateType},
            reward_certificate::NUM_SLOTS_FOR_REWARD,
        },
        bitvec::prelude::*,
        rand::seq::IndexedRandom,
        solana_account::{Account, ReadableAccount, WritableAccount},
        solana_bls_signatures::{BLS_SIGNATURE_AFFINE_SIZE, Signature as BLSSignature},
        solana_cluster_type::ClusterType,
        solana_epoch_schedule::EpochSchedule,
        solana_fee_calculator::FeeRateGovernor,
        solana_genesis_config::GenesisConfig,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_leader_schedule::SlotLeader,
        solana_native_token::LAMPORTS_PER_SOL,
        solana_rent::Rent,
        solana_signer::Signer,
        solana_signer_store::encode_base2,
        solana_stake_interface::state::StakeStateV2,
        solana_vote_interface::state::{VoteStateV4, VoteStateVersions},
        std::{
            collections::HashMap,
            sync::{Arc, RwLock},
        },
        test_case::test_matrix,
    };

    fn vote_state_from_account(account: &AccountSharedData) -> VoteStateHandler {
        let versions = bincode::deserialize(account.data()).unwrap();
        VoteStateHandler::try_new_from_vote_state_versions(versions).unwrap()
    }

    fn vote_state_from_bank(bank: &Bank, vote_pubkey: &Pubkey) -> VoteStateHandler {
        let vote_accounts = bank.vote_accounts();
        let (_, vote_account) = vote_accounts.get(vote_pubkey).unwrap();
        vote_state_from_account(vote_account.account())
    }

    fn build_fast_finalization_cert(
        bank: &Bank,
        signing_ranks: &[usize],
    ) -> ValidatedBlockFinalizationCert {
        let slot = bank.slot();
        let block_id = Hash::new_unique();
        let cert_type = CertificateType::FinalizeFast(slot, block_id);
        let max_rank = signing_ranks.iter().copied().max().unwrap_or(0);
        let mut bitvec = BitVec::<u8, Lsb0>::repeat(false, max_rank.saturating_add(1));
        for &rank in signing_ranks {
            bitvec.set(rank, true);
        }
        let bitmap = encode_base2(&bitvec).unwrap();

        let cert = Certificate {
            cert_type,
            signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
            bitmap,
        };
        ValidatedBlockFinalizationCert::from_validated_fast(cert, bank)
    }

    #[test]
    fn calculate_voting_reward_does_not_panic() {
        // the current circulating supply is about 566M.  The most extreme numbers are when all of
        // it is staked by a single validator.
        let circulating_supply = 566_000_000 * LAMPORTS_PER_SOL;

        let bank_forks = BankForks::new_rw_arc(Bank::new_for_tests(&GenesisConfig::default()));
        let bank = bank_forks.read().unwrap().root_bank();
        let validator_rewards_lamports =
            bank.calculate_epoch_inflation_rewards(circulating_supply, 1);

        let epoch_state = EpochInflationState {
            slots_per_epoch: bank.epoch_schedule.slots_per_epoch,
            max_possible_validator_reward: validator_rewards_lamports,
            epoch: 1234,
        };

        calculate_reward(&epoch_state, circulating_supply, circulating_supply);
    }

    #[test]
    fn increment_credits_works() {
        let mut handle = VoteStateHandler::default_v4();
        let epoch = 1234;
        let credits = 543432;
        handle.increment_credits(epoch, credits);
        let (got_epoch, got_final_credits, got_initial_credits) =
            *handle.epoch_credits().last().unwrap();
        assert_eq!(got_epoch, epoch);
        assert_eq!(got_final_credits, credits);
        assert_eq!(got_initial_credits, 0);
    }

    fn calc_reward_for_test(
        prev_bank: &Bank,
        bank: &Bank,
        total_stake: u64,
        stake_voted: u64,
    ) -> u64 {
        let epoch_inflation =
            bank.calculate_epoch_inflation_rewards(prev_bank.capitalization(), prev_bank.epoch());
        let numerator = epoch_inflation as u128 * stake_voted as u128;
        let denominator = bank.epoch_schedule.slots_per_epoch as u128 * total_stake as u128;
        let reward: u64 = (numerator / denominator).try_into().unwrap();
        reward / 2
    }

    #[test]
    fn calculate_and_pay_works() {
        let num_validators = 100;
        let per_validator_stake = LAMPORTS_PER_SOL * 100;
        let num_validators_to_reward = 10;

        let validator_keypairs = (0..num_validators)
            .map(|_| ValidatorVoteKeypairs::new_rand())
            .collect::<Vec<_>>();
        let mut genesis_config = create_genesis_config_with_alpenglow_vote_accounts(
            1_000_000_000,
            &validator_keypairs,
            vec![per_validator_stake; validator_keypairs.len()],
        )
        .genesis_config;
        genesis_config.epoch_schedule = EpochSchedule::without_warmup();
        genesis_config.rent = Rent::default();

        let validator_keypairs_to_reward = validator_keypairs
            .choose_multiple(&mut rand::rng(), num_validators_to_reward as usize)
            .collect::<Vec<_>>();

        let validator_pubkeys_to_reward = validator_keypairs_to_reward
            .iter()
            .map(|v| v.vote_keypair.pubkey())
            .collect::<Vec<_>>();
        let leader_vote_pubkey = validator_keypairs_to_reward[0].vote_keypair.pubkey();
        let leader_node_pubkey = validator_keypairs_to_reward[0].node_keypair.pubkey();
        let slot_leader = SlotLeader {
            id: leader_node_pubkey,
            vote_address: leader_vote_pubkey,
        };

        let bank_forks = BankForks::new_rw_arc(Bank::new_for_tests(&genesis_config));
        let prev_bank = bank_forks.read().unwrap().root_bank();
        let current_slot = prev_bank
            .epoch_schedule
            .get_first_slot_in_epoch(prev_bank.epoch() + 1)
            + NUM_SLOTS_FOR_REWARD;
        let bank = Bank::new_from_parent(prev_bank.clone(), slot_leader, current_slot);
        let reward_slot = current_slot - NUM_SLOTS_FOR_REWARD;

        calc_vote_rewards_update_vote_states(
            &bank,
            Some(ValidatedRewardCert::new_for_tests(
                reward_slot,
                validator_pubkeys_to_reward.clone(),
            )),
            None,
        )
        .unwrap();

        let vote_accounts = bank.vote_accounts();
        let rewards = validator_pubkeys_to_reward
            .iter()
            .map(|validator| {
                let (_, vote_account) = vote_accounts.get(validator).unwrap();
                let vote_state = vote_state_from_account(vote_account.account());
                assert_eq!(vote_state.epoch_credits().len(), 1);
                let got_reward = vote_state.epoch_credits()[0].1;
                let total_stake = bank
                    .epoch_stakes_from_slot(reward_slot)
                    .unwrap()
                    .total_stake();
                let expected_validator_reward =
                    calc_reward_for_test(&prev_bank, &bank, total_stake, per_validator_stake);
                if *validator != leader_vote_pubkey {
                    assert_eq!(got_reward, expected_validator_reward);
                }
                got_reward
            })
            .collect::<Vec<_>>();
        let expected_leader_reward = rewards.last().unwrap()
            * validator_pubkeys_to_reward.len() as u64
            + rewards.last().unwrap();
        assert_eq!(expected_leader_reward, rewards[0]);
    }

    #[test]
    fn calculate_and_pay_sets_root_slot_for_signer_in_final_cert() {
        let validator_keypairs = (0..4)
            .map(|_| ValidatorVoteKeypairs::new_rand())
            .collect::<Vec<_>>();
        let per_validator_stake = LAMPORTS_PER_SOL * 100;
        let mut genesis_config = create_genesis_config_with_alpenglow_vote_accounts(
            1_000_000_000,
            &validator_keypairs,
            vec![per_validator_stake; validator_keypairs.len()],
        )
        .genesis_config;
        genesis_config.epoch_schedule = EpochSchedule::without_warmup();
        genesis_config.rent = Rent::default();

        let leader_vote_pubkey = validator_keypairs[0].vote_keypair.pubkey();
        let leader_node_pubkey = validator_keypairs[0].node_keypair.pubkey();
        let slot_leader = SlotLeader {
            id: leader_node_pubkey,
            vote_address: leader_vote_pubkey,
        };
        let target_vote_pubkey = validator_keypairs[1].vote_keypair.pubkey();

        let bank_forks = BankForks::new_rw_arc(Bank::new_for_tests(&genesis_config));
        let prev_bank = bank_forks.read().unwrap().root_bank();
        let current_slot = prev_bank
            .epoch_schedule
            .get_first_slot_in_epoch(prev_bank.epoch() + 1)
            + NUM_SLOTS_FOR_REWARD;
        let bank = Bank::new_from_parent(prev_bank.clone(), slot_leader, current_slot);
        let reward_slot = current_slot - NUM_SLOTS_FOR_REWARD;

        let cert_rank = {
            let rank_map = bank
                .epoch_stakes_from_slot(bank.slot())
                .unwrap()
                .bls_pubkey_to_rank_map();
            (0..rank_map.len())
                .find_map(|rank| {
                    rank_map.get_pubkey_stake_entry(rank).and_then(|entry| {
                        (entry.vote_account_pubkey == target_vote_pubkey).then_some(rank)
                    })
                })
                .unwrap()
        };
        let final_cert = build_fast_finalization_cert(&bank, &[cert_rank]);
        let (signers, finalize_cert, _) = final_cert.clone().into_parts();
        let final_cert_input = Some((&signers, finalize_cert.cert_type.slot()));

        calc_vote_rewards_update_vote_states(
            &bank,
            Some(ValidatedRewardCert::new_for_tests(
                reward_slot,
                vec![target_vote_pubkey],
            )),
            final_cert_input,
        )
        .unwrap();

        let handle = vote_state_from_bank(&bank, &target_vote_pubkey);
        assert_eq!(handle.root_slot(), Some(final_cert.slot()));
        assert_eq!(handle.votes().len(), 1);
        assert_eq!(
            handle.votes().front().unwrap().lockout.slot(),
            final_cert.slot().max(reward_slot)
        );
    }

    #[test]
    fn calculate_and_pay_uses_reward_slot_vote_when_signer_absent_from_final_cert() {
        let validator_keypairs = (0..4)
            .map(|_| ValidatorVoteKeypairs::new_rand())
            .collect::<Vec<_>>();
        let per_validator_stake = LAMPORTS_PER_SOL * 100;
        let mut genesis_config = create_genesis_config_with_alpenglow_vote_accounts(
            1_000_000_000,
            &validator_keypairs,
            vec![per_validator_stake; validator_keypairs.len()],
        )
        .genesis_config;
        genesis_config.epoch_schedule = EpochSchedule::without_warmup();
        genesis_config.rent = Rent::default();

        let leader_node_pubkey = validator_keypairs[0].node_keypair.pubkey();
        let leader_vote_pubkey = validator_keypairs[0].vote_keypair.pubkey();
        let target_vote_pubkey = validator_keypairs[1].vote_keypair.pubkey();
        let slot_leader = SlotLeader {
            id: leader_node_pubkey,
            vote_address: leader_vote_pubkey,
        };
        let non_target_vote_pubkey = validator_keypairs[2].vote_keypair.pubkey();

        let bank_forks = BankForks::new_rw_arc(Bank::new_for_tests(&genesis_config));
        let prev_bank = bank_forks.read().unwrap().root_bank();
        let current_slot = prev_bank
            .epoch_schedule
            .get_first_slot_in_epoch(prev_bank.epoch() + 1)
            + NUM_SLOTS_FOR_REWARD;
        let bank = Bank::new_from_parent(prev_bank.clone(), slot_leader, current_slot);
        let reward_slot = current_slot - NUM_SLOTS_FOR_REWARD;

        let cert_rank = {
            let rank_map = bank
                .epoch_stakes_from_slot(bank.slot())
                .unwrap()
                .bls_pubkey_to_rank_map();
            (0..rank_map.len())
                .find_map(|rank| {
                    rank_map.get_pubkey_stake_entry(rank).and_then(|entry| {
                        (entry.vote_account_pubkey == non_target_vote_pubkey).then_some(rank)
                    })
                })
                .unwrap()
        };
        let final_cert = build_fast_finalization_cert(&bank, &[cert_rank]);
        let (signers, finalize_cert, _) = final_cert.into_parts();
        let final_cert_input = Some((&signers, finalize_cert.cert_type.slot()));

        calc_vote_rewards_update_vote_states(
            &bank,
            Some(ValidatedRewardCert::new_for_tests(
                reward_slot,
                vec![target_vote_pubkey],
            )),
            final_cert_input,
        )
        .unwrap();

        let vote_state = vote_state_from_bank(&bank, &target_vote_pubkey);
        assert_eq!(vote_state.root_slot(), None);
        assert_eq!(vote_state.votes().len(), 1);
        assert_eq!(
            vote_state.votes().front().unwrap().lockout.slot(),
            reward_slot
        );
    }

    #[test]
    fn leader_with_multiple_vote_accounts_not_paid() {
        let num_validators = 5;
        let per_validator_stake = LAMPORTS_PER_SOL * 100;
        let node_keypair = Keypair::new();

        let validator_keypairs = (0..num_validators)
            .map(|_| {
                ValidatorVoteKeypairs::new(
                    node_keypair.insecure_clone(),
                    Keypair::new(),
                    Keypair::new(),
                )
            })
            .collect::<Vec<_>>();
        let mut genesis_config = create_genesis_config_with_alpenglow_vote_accounts(
            1_000_000_000,
            &validator_keypairs,
            vec![per_validator_stake; validator_keypairs.len()],
        )
        .genesis_config;
        genesis_config.epoch_schedule = EpochSchedule::without_warmup();
        genesis_config.rent = Rent::default();
        let bank_forks = BankForks::new_rw_arc(Bank::new_for_tests(&genesis_config));
        let prev_bank = bank_forks.read().unwrap().root_bank();
        let current_slot = prev_bank
            .epoch_schedule
            .get_first_slot_in_epoch(prev_bank.epoch() + 1)
            + NUM_SLOTS_FOR_REWARD;

        let node_pubkey = node_keypair.pubkey();
        let vote_pubkey = validator_keypairs[0].vote_keypair.pubkey();
        let slot_leader = SlotLeader {
            id: node_pubkey,
            vote_address: vote_pubkey,
        };
        let bank = Bank::new_from_parent(prev_bank.clone(), slot_leader, current_slot);
        let reward_slot = current_slot - NUM_SLOTS_FOR_REWARD;

        calc_vote_rewards_update_vote_states(
            &bank,
            Some(ValidatedRewardCert::new_for_tests(
                reward_slot,
                vec![vote_pubkey],
            )),
            None,
        )
        .unwrap();
        let vote_accounts = bank.vote_accounts();
        for (add, (_, vote_account)) in vote_accounts.iter() {
            let vote_state = vote_state_from_account(vote_account.account());
            if add == &vote_pubkey {
                assert!(!vote_state.epoch_credits().is_empty());
            } else {
                assert!(vote_state.epoch_credits().is_empty());
            }
        }
    }

    fn set_commission(
        genesis_config: &mut GenesisConfig,
        validators: &[ValidatorVoteKeypairs],
        commission_bps: u16,
    ) {
        for validator in validators {
            let vote_pubkey = validator.vote_keypair.pubkey();
            let account = genesis_config.accounts.get_mut(&vote_pubkey).unwrap();
            let vote_state_versions = bincode::deserialize(&account.data).unwrap();
            let VoteStateVersions::V4(mut vote_state) = vote_state_versions else {
                panic!();
            };
            vote_state.inflation_rewards_commission_bps = commission_bps;
            VoteStateV4::serialize(
                &VoteStateVersions::V4(vote_state),
                account.data_as_mut_slice(),
            )
            .unwrap();
        }
    }

    #[derive(Debug)]
    struct Staker {
        lamports: u64,
        expected_rewards: u64,
    }

    impl Staker {
        /// Creates a new `Staker`.
        ///
        /// Returns a `Staker` and the share of the rewards for the voter.
        fn new(
            rent: &Rent,
            bank: &Bank,
            commission_bps: u16,
            pubkey: Pubkey,
            validator_reward: u64,
            validator_stake: u64,
        ) -> (Self, u64) {
            let rent_exempt_reserve = rent.minimum_balance(StakeStateV2::size_of());
            let lamports = bank.get_account(&pubkey).unwrap().lamports();
            if lamports <= LAMPORTS_PER_SOL + rent_exempt_reserve {
                return (
                    Self {
                        lamports,
                        expected_rewards: 0,
                    },
                    0,
                );
            }
            let stake = lamports - rent_exempt_reserve;
            let stake_weighted_reward = validator_reward * stake / validator_stake;
            let (voter_reward, staker_reward, is_split) =
                commission_split(commission_bps, stake_weighted_reward);
            assert!(is_split);
            (
                Self {
                    lamports,
                    expected_rewards: staker_reward,
                },
                voter_reward,
            )
        }
    }

    #[derive(Debug)]
    struct StateEntry {
        voter_lamports: u64,
        voter_expected_reward: u64,
        stakers: HashMap<Pubkey, Staker>,
    }

    impl StateEntry {
        /// Creates a new `StateEntry` for when the voter is not the block leader.
        ///
        /// Returns a `StateEntry` and the share of the rewards for the block leader.
        fn new_not_leader(
            bank: &Bank,
            reward_epoch: Epoch,
            voter_pubkey: Pubkey,
            staker_pubkeys: &[Pubkey],
            rent: &Rent,
            commission_bps: u16,
            num_reward_slots: u64,
        ) -> (Self, u64) {
            let rent_exempt_reserve = rent.minimum_balance(StakeStateV2::size_of());
            let voter_lamports = bank.get_account(&voter_pubkey).unwrap().lamports();
            let validator_stake = staker_pubkeys
                .iter()
                .map(|pubkey| {
                    let lamports = bank.get_account(pubkey).unwrap().lamports();
                    lamports - rent_exempt_reserve
                })
                .sum::<u64>();

            let epoch_state = EpochInflationAccountState::new_from_bank(bank)
                .unwrap()
                .get_epoch_state(bank.epoch())
                .unwrap();
            let total_stake = bank.epoch_stakes(reward_epoch).unwrap().total_stake();
            let (validator_reward, leader_reward) =
                calculate_reward(&epoch_state, total_stake, validator_stake);
            let vote_rewards = validator_reward * num_reward_slots;
            let leader_reward = leader_reward * num_reward_slots;

            let mut voter_expected_reward = 0;
            let mut stakers = HashMap::new();
            for staker_pubkey in staker_pubkeys {
                let (staker, voter_reward) = Staker::new(
                    rent,
                    bank,
                    commission_bps,
                    *staker_pubkey,
                    vote_rewards,
                    validator_stake,
                );
                voter_expected_reward += voter_reward;
                stakers.insert(*staker_pubkey, staker);
            }

            (
                Self {
                    voter_lamports,
                    stakers,
                    voter_expected_reward,
                },
                leader_reward,
            )
        }

        /// Creates a `StateEntry` for when the validator is the leader.
        fn new_leader(
            bank: &Bank,
            reward_epoch: Epoch,
            voter_pubkey: Pubkey,
            staker_pubkeys: &[Pubkey],
            rent: &Rent,
            commission_bps: u16,
            num_reward_slots: u64,
            add_leader_reward: u64,
        ) -> Self {
            let rent_exempt_reserve = rent.minimum_balance(StakeStateV2::size_of());
            let voter_lamports = bank.get_account(&voter_pubkey).unwrap().lamports();
            let validator_stake = staker_pubkeys
                .iter()
                .map(|pubkey| {
                    let lamports = bank.get_account(pubkey).unwrap().lamports();
                    lamports - rent_exempt_reserve
                })
                .sum::<u64>();

            let epoch_state = EpochInflationAccountState::new_from_bank(bank)
                .unwrap()
                .get_epoch_state(bank.epoch())
                .unwrap();
            let total_stake = bank.epoch_stakes(reward_epoch).unwrap().total_stake();
            let (validator_reward, leader_reward) =
                calculate_reward(&epoch_state, total_stake, validator_stake);
            let vote_rewards =
                (validator_reward + leader_reward) * num_reward_slots + add_leader_reward;

            let mut voter_expected_reward = 0;
            let mut stakers = HashMap::new();
            for staker_pubkey in staker_pubkeys {
                let (staker, voter_reward) = Staker::new(
                    rent,
                    bank,
                    commission_bps,
                    *staker_pubkey,
                    vote_rewards,
                    validator_stake,
                );
                voter_expected_reward += voter_reward;
                stakers.insert(*staker_pubkey, staker);
            }

            Self {
                voter_lamports,
                stakers,
                voter_expected_reward,
            }
        }
    }

    #[derive(Debug)]
    struct State {
        entries: HashMap<Pubkey, StateEntry>,
        epoch: Epoch,
    }

    impl State {
        fn new(
            bank: &Bank,
            reward_epoch: Epoch,
            staker_pubkeys: &HashMap<Pubkey, Vec<Pubkey>>,
            rent: &Rent,
            leader: Pubkey,
            commission_bps: u16,
            num_reward_slots: u64,
        ) -> Self {
            let mut entries = HashMap::new();

            let mut add_leader_reward = 0;
            for (voter_pubkey, stakers) in staker_pubkeys {
                if voter_pubkey == &leader {
                    continue;
                }
                let (state_entry, leader_reward) = StateEntry::new_not_leader(
                    bank,
                    reward_epoch,
                    *voter_pubkey,
                    stakers,
                    rent,
                    commission_bps,
                    num_reward_slots,
                );
                add_leader_reward += leader_reward;
                entries.insert(*voter_pubkey, state_entry);
            }

            if let Some(stakers) = staker_pubkeys.get(&leader) {
                let state_entry = StateEntry::new_leader(
                    bank,
                    reward_epoch,
                    leader,
                    stakers,
                    rent,
                    commission_bps,
                    num_reward_slots,
                    add_leader_reward,
                );
                entries.insert(leader, state_entry);
            }

            Self {
                entries,
                epoch: bank.epoch(),
            }
        }
    }

    #[derive(Debug)]
    struct RewardState {
        vote_pubkey: Pubkey,
        stake_prev_lamports: u64,
        vote_prev_lamports: u64,
        expected_reward: u64,
    }

    impl RewardState {
        fn new(keypair: &ValidatorVoteKeypairs, bank: &Bank) -> Self {
            let stake_pubkey = keypair.stake_keypair.pubkey();
            let vote_pubkey = keypair.vote_keypair.pubkey();

            let stake_account = bank.get_account(&stake_pubkey).unwrap();
            let vote_account = bank.get_account(&vote_pubkey).unwrap();

            let stake_prev_lamports = stake_account.lamports();
            let vote_prev_lamports = vote_account.lamports();

            let vote_account = bank.get_account(&vote_pubkey).unwrap();
            let vote_state = vote_state_from_account(&vote_account);
            assert_eq!(vote_state.epoch_credits().len(), 1);
            let (epoch, final_credit, initial_credit) = vote_state.epoch_credits()[0];

            assert_eq!(epoch, bank.epoch());
            assert_eq!(initial_credit, 0);
            let expected_reward = final_credit;

            Self {
                vote_pubkey,
                stake_prev_lamports,
                vote_prev_lamports,
                expected_reward,
            }
        }
    }

    #[derive(Debug)]
    struct ValidatorRewardState {
        prev_lamports: u64,
        expected_rewards: u64,
    }

    fn initial_state(
        validator_keypairs: &[ValidatorVoteKeypairs],
        commission_bps: u16,
    ) -> (Arc<Bank>, Arc<RwLock<BankForks>>) {
        let per_validator_stake = LAMPORTS_PER_SOL;
        let mut genesis_config_info = create_genesis_config_with_alpenglow_vote_accounts(
            1_000_000_000,
            validator_keypairs,
            vec![per_validator_stake; validator_keypairs.len()],
        );
        genesis_config_info.genesis_config.epoch_schedule = EpochSchedule::without_warmup();
        genesis_config_info.genesis_config.rent = Rent::default();
        set_commission(
            &mut genesis_config_info.genesis_config,
            validator_keypairs,
            commission_bps,
        );

        let (bank_epoch0, bank_forks) =
            Bank::new_with_bank_forks_for_tests(&genesis_config_info.genesis_config);
        assert_eq!(bank_epoch0.epoch(), 0);

        let epoch1_slot = bank_epoch0.epoch_schedule.get_first_slot_in_epoch(1);
        let bank_epoch1 = Arc::new(Bank::new_from_parent(
            bank_epoch0,
            SlotLeader::new_unique(),
            epoch1_slot,
        ));
        assert_eq!(bank_epoch1.epoch(), 1);
        (bank_epoch1, bank_forks)
    }

    fn reward_validators(
        bank: Arc<Bank>,
        validators: &[ValidatorVoteKeypairs],
        num_reward_slots: u64,
    ) -> (Arc<Bank>, HashMap<Pubkey, RewardState>) {
        let validators_to_reward = validators
            .iter()
            .map(|k| k.vote_keypair.pubkey())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();

        let mut looping_bank = bank;
        for _ in 0..num_reward_slots {
            let reward_cert = ValidatedRewardCert::new_for_tests(
                looping_bank.slot() - 100,
                validators_to_reward.clone(),
            );
            calc_vote_rewards_update_vote_states(&looping_bank, Some(reward_cert), None).unwrap();

            let leader = *looping_bank.leader();
            let slot = looping_bank.slot() + 1;
            looping_bank = Arc::new(Bank::new_from_parent(looping_bank, leader, slot));
        }

        let map = validators
            .iter()
            .map(|keypair| {
                let stake_pubkey = keypair.stake_keypair.pubkey();
                let reward_state = RewardState::new(keypair, &looping_bank);
                (stake_pubkey, reward_state)
            })
            .collect::<HashMap<_, _>>();
        (looping_bank, map)
    }

    fn test_vote_reward_payout_impl(
        validators: &[ValidatorVoteKeypairs],
        leader: SlotLeader,
        commission_bps: u16,
        initial_bank: Option<(Arc<Bank>, Arc<RwLock<BankForks>>)>,
        num_reward_slots: u64,
    ) -> (Arc<Bank>, HashMap<Pubkey, RewardState>) {
        let (initial_bank, _bank_forks) =
            initial_bank.unwrap_or_else(|| initial_state(validators, commission_bps));

        let reward_epoch = initial_bank.epoch() + 1;
        let reward_epoch_slot = initial_bank
            .epoch_schedule
            .get_first_slot_in_epoch(reward_epoch);
        let reward_bank = Arc::new(Bank::new_from_parent(
            initial_bank,
            leader,
            reward_epoch_slot,
        ));
        assert_eq!(reward_bank.epoch(), reward_epoch);

        let (reward_bank, rewarded_validators) =
            reward_validators(reward_bank, validators, num_reward_slots);

        let payout_epoch = reward_bank.epoch() + 1;
        let payout_epoch_slot = reward_bank
            .epoch_schedule
            .get_first_slot_in_epoch(payout_epoch);
        let payout_bank = Bank::new_from_parent(reward_bank, leader, payout_epoch_slot);
        assert_eq!(payout_bank.epoch(), payout_epoch);

        // Need to progress banks a few times for the rewards to be paid.
        let mut prev_bank = Arc::new(payout_bank);
        for i in 0..10 {
            let leader = *prev_bank.leader();
            let slot = prev_bank.slot() + 1 + i;
            prev_bank = Arc::new(Bank::new_from_parent(prev_bank, leader, slot));
        }

        (prev_bank, rewarded_validators)
    }

    #[test_matrix([true, false], [1_000, 5_000])]
    fn test_vote_reward_payout(pay_leader: bool, commission_bps: u16) {
        let num_validators = 3;
        let num_reward_slots = 10;
        let validators = (0..num_validators)
            .map(|_| ValidatorVoteKeypairs::new_rand())
            .collect::<Vec<_>>();
        let leader = if pay_leader {
            let vote_pubkey = validators[0].vote_keypair.pubkey();
            let node_pubkey = validators[0].node_keypair.pubkey();
            SlotLeader {
                id: node_pubkey,
                vote_address: vote_pubkey,
            }
        } else {
            SlotLeader::new_unique()
        };
        let (final_bank, rewarded_validators) = test_vote_reward_payout_impl(
            &validators,
            leader,
            commission_bps,
            None,
            num_reward_slots,
        );
        let mut voter_rewards = HashMap::new();

        for (stake_pubkey, reward_state) in rewarded_validators {
            let stake_account = final_bank.get_account(&stake_pubkey).unwrap();
            let stake_cur = stake_account.lamports();

            let (vote_expected_reward, stake_expected_reward, is_split) =
                commission_split(commission_bps, reward_state.expected_reward);

            let validator_reward_state =
                voter_rewards
                    .entry(reward_state.vote_pubkey)
                    .or_insert(ValidatorRewardState {
                        prev_lamports: reward_state.vote_prev_lamports,
                        expected_rewards: 0,
                    });
            validator_reward_state.expected_rewards += vote_expected_reward;

            assert!(is_split);
            assert_ne!(
                stake_expected_reward, 0,
                "stake_expected_reward {stake_expected_reward} should not be 0"
            );
            assert_ne!(
                vote_expected_reward, 0,
                "vote_expected_reward {vote_expected_reward} should not be 0"
            );
            assert_eq!(
                stake_cur - reward_state.stake_prev_lamports,
                stake_expected_reward
            );

            // Due to rounding issues, off by 1 errors are possible.
            let total_reward = stake_expected_reward + vote_expected_reward;
            assert!(
                total_reward.max(reward_state.expected_reward)
                    - total_reward.min(reward_state.expected_reward)
                    <= 1
            );
        }

        for (vote_pubkey, state) in voter_rewards {
            let vote_account = final_bank.get_account(&vote_pubkey).unwrap();
            let vote_cur = vote_account.lamports();
            assert_eq!(
                vote_cur + VAT_TO_BURN_PER_EPOCH - state.prev_lamports,
                state.expected_rewards
            );
        }
    }

    #[test_matrix([true, false], [1_000, 5_000])]
    fn test_multiple_delegators(pay_leader: bool, commission_bps: u16) {
        let num_validators = 2;
        let num_add_stakers = 5;
        let num_reward_slots = 10;
        let lamports = LAMPORTS_PER_SOL * 20;
        let mint_keypair = Keypair::new();
        let validators = (0..num_validators)
            .map(|_| ValidatorVoteKeypairs::new_rand())
            .collect::<Vec<_>>();
        let leader = if pay_leader {
            let vote_pubkey = validators[0].vote_keypair.pubkey();
            let node_pubkey = validators[0].node_keypair.pubkey();
            SlotLeader {
                id: node_pubkey,
                vote_address: vote_pubkey,
            }
        } else {
            SlotLeader::new_unique()
        };
        let mut genesis_config = create_genesis_config_with_leader_ex(
            lamports,
            &mint_keypair.pubkey(),
            &validators[0].node_keypair.pubkey(),
            &validators[0].vote_keypair.pubkey(),
            &validators[0].stake_keypair.pubkey(),
            Some(validators[0].bls_keypair.public.to_bytes_compressed()),
            lamports,
            lamports,
            FeeRateGovernor::new(0, 0),
            Rent::default(),
            ClusterType::Development,
            &FeatureSet::all_enabled(),
            vec![],
        );
        genesis_config.epoch_schedule = EpochSchedule::without_warmup();
        activate_all_features_alpenglow(&mut genesis_config);
        for (ind, keypair) in validators.iter().enumerate().skip(1) {
            let node_pubkey = keypair.node_keypair.pubkey();
            let vote_pubkey = keypair.vote_keypair.pubkey();
            let stake_pubkey = keypair.stake_keypair.pubkey();
            let bls_pubkey = Some(keypair.bls_keypair.public.to_bytes_compressed());
            let lamports = lamports + ind as u64 * LAMPORTS_PER_SOL;
            let accounts = create_validator(
                &genesis_config.rent,
                node_pubkey,
                lamports,
                vote_pubkey,
                lamports,
                stake_pubkey,
                lamports,
                bls_pubkey,
            )
            .into_iter()
            .map(|(pubkey, account)| (pubkey, Account::from(account)));
            genesis_config.accounts.extend(accounts);
        }
        set_commission(&mut genesis_config, &validators, commission_bps);

        let vote_account = genesis_config
            .accounts
            .get(&validators[0].vote_keypair.pubkey())
            .unwrap()
            .clone()
            .into();

        let staker_keypairs = (0..num_add_stakers)
            .map(|_| Keypair::new())
            .collect::<Vec<_>>();
        for (ind, keypair) in staker_keypairs.iter().enumerate() {
            let stake_pubkey = keypair.pubkey();
            let account = Account::from(stake_utils::create_stake_account(
                &stake_pubkey,
                &validators[0].vote_keypair.pubkey(),
                &vote_account,
                &genesis_config.rent,
                lamports + (ind as u64 + 1) * lamports,
            ));
            genesis_config.accounts.insert(stake_pubkey, account);
        }

        let (bank_epoch0, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        assert_eq!(bank_epoch0.epoch(), 0);

        let epoch1_slot = bank_epoch0.epoch_schedule.get_first_slot_in_epoch(1);
        let initial_bank = Arc::new(Bank::new_from_parent(
            bank_epoch0,
            SlotLeader::new_unique(),
            epoch1_slot,
        ));
        assert_eq!(initial_bank.epoch(), 1);

        let staker_pubkeys = {
            let mut staker_pubkeys = validators
                .iter()
                .map(|keypair| {
                    (
                        keypair.vote_keypair.pubkey(),
                        vec![keypair.stake_keypair.pubkey()],
                    )
                })
                .collect::<HashMap<_, _>>();
            for staker_keypair in &staker_keypairs {
                let staker_pubkey = staker_keypair.pubkey();
                staker_pubkeys
                    .get_mut(&validators[0].vote_keypair.pubkey())
                    .unwrap()
                    .push(staker_pubkey);
            }
            staker_pubkeys
        };

        let reward_epoch = initial_bank.epoch() + 1;

        let prev_state = State::new(
            &initial_bank,
            reward_epoch,
            &staker_pubkeys,
            &genesis_config.rent,
            leader.vote_address,
            commission_bps,
            num_reward_slots,
        );

        let (final_bank, _) = test_vote_reward_payout_impl(
            &validators,
            leader,
            commission_bps,
            Some((initial_bank, bank_forks)),
            num_reward_slots,
        );

        let final_state = State::new(
            &final_bank,
            reward_epoch,
            &staker_pubkeys,
            &genesis_config.rent,
            leader.vote_address,
            commission_bps,
            num_reward_slots,
        );

        for (vote_pubkey, prev_entry) in &prev_state.entries {
            let final_entry = final_state.entries.get(vote_pubkey).unwrap();
            for (staker_pubkey, prev_staker) in &prev_entry.stakers {
                let final_staker = final_entry.stakers.get(staker_pubkey).unwrap();
                let diff = final_staker.lamports - prev_staker.lamports;
                assert_eq!(
                    diff, prev_staker.expected_rewards,
                    "before={} after={} diff={diff} expected={}",
                    prev_staker.lamports, final_staker.lamports, prev_staker.expected_rewards
                );
            }

            let voter_diff = final_entry.voter_lamports
                + VAT_TO_BURN_PER_EPOCH * (final_state.epoch - prev_state.epoch)
                - prev_entry.voter_lamports;
            // Due to rounding issues, off by 1 errors are possible.
            assert!(
                voter_diff.abs_diff(prev_entry.voter_expected_reward) <= 1,
                "final_lamports={} prev_lamports={} diff={voter_diff} expected={}",
                final_entry.voter_lamports,
                prev_entry.voter_lamports,
                prev_entry.voter_expected_reward
            );
        }
    }
}
