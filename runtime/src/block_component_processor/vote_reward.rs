use {
    crate::{bank::Bank, validated_reward_certificate::ValidatedRewardCert},
    bincode::Error as BincodeError,
    epoch_inflation_account_state::{EpochInflationAccountState, EpochInflationState},
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
#[derive(Debug, PartialEq, Eq, Error)]
pub(super) enum Error {
    #[error("missing EpochInflationAccountState for current slot {current_slot}")]
    MissingEpochInflationAccountState { current_slot: Slot },
    #[error("missing epoch stakes for reward_slot {reward_slot} in current_slot {current_slot}")]
    MissingEpochStakes {
        reward_slot: Slot,
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
    #[error(
        "missing validator stake info for reward epoch {reward_epoch} in current_slot \
         {current_slot}"
    )]
    NoEpochValidatorStake {
        reward_epoch: Epoch,
        current_slot: Slot,
    },
    #[error("Missing rank map for reward cert or final cert input")]
    RankMapNotFound,
}

struct MyVoteStateHandler {
    handler: VoteStateHandler,
    lamports: u64,
    space: usize,
    owner: Pubkey,
}

impl MyVoteStateHandler {
    fn try_new(
        vote_accounts: &HashMap<Pubkey, (u64, VoteAccount)>,
        vote_pubkey: Pubkey,
    ) -> Result<Self, UpdateAccountError> {
        let account = vote_accounts
            .get(&vote_pubkey)
            .ok_or(UpdateAccountError::AccountNotFound)?
            .1
            .account();
        let versions =
            bincode::deserialize(account.data()).map_err(UpdateAccountError::DeserializeFailed)?;
        let handler = VoteStateHandler::try_new_from_vote_state_versions(versions)
            .map_err(UpdateAccountError::HandlerConversionFailed)?;
        Ok(Self {
            handler,
            lamports: account.lamports(),
            space: account.data().len(),
            owner: *account.owner(),
        })
    }

    fn serialize(self) -> Result<AccountSharedData, UpdateAccountError> {
        let mut updated_account = AccountSharedData::new(self.lamports, self.space, &self.owner);
        self.handler
            .serialize_into(updated_account.data_as_mut_slice())
            .map_err(UpdateAccountError::SerializeFailed)?;
        Ok(updated_account)
    }
}

/// Common state required to pay rewards to different accounts.
#[derive(Debug)]
struct RewardState<'a> {
    reward_epoch: Epoch,
    reward_slot: Slot,
    current_slot: Slot,
    leader_vote_pubkey: Pubkey,
    accounts: &'a HashMap<Pubkey, (u64, VoteAccount)>,
    total_stake: u64,
    epoch_inflation_state: EpochInflationState,
}

impl<'a> RewardState<'a> {
    fn new(bank: &'a Bank, reward_slot: Slot) -> Result<Self, Error> {
        let current_slot = bank.slot();
        let (accounts, total_stake) = {
            let epoch_stakes =
                bank.epoch_stakes_from_slot(reward_slot)
                    .ok_or(Error::MissingEpochStakes {
                        reward_slot,
                        current_slot,
                    })?;
            (
                epoch_stakes.stakes().vote_accounts().as_ref(),
                epoch_stakes.total_stake(),
            )
        };
        // This assumes that if the epoch_schedule ever changes, the new schedule will maintain correct
        // info about older slots as well.
        let reward_epoch = bank.epoch_schedule.get_epoch(reward_slot);
        let epoch_inflation_state = {
            let epoch_inflation_account_state = EpochInflationAccountState::new_from_bank(bank);
            // This function should only be called after alpenglow is active and the slot in the the epoch
            // that activated Alpenglow should have created the account.
            debug_assert!(epoch_inflation_account_state.is_some());
            epoch_inflation_account_state
                .ok_or(Error::MissingEpochInflationAccountState { current_slot })?
                .get_epoch_state(reward_epoch)
                .ok_or(Error::NoEpochValidatorStake {
                    reward_epoch,
                    current_slot,
                })?
        };
        Ok(Self {
            reward_epoch,
            reward_slot,
            current_slot,
            leader_vote_pubkey: bank.leader().vote_address,
            accounts,
            total_stake,
            epoch_inflation_state,
        })
    }

    /// Calculates rewards for the `validator`.
    ///
    /// Returns Ok(Some(RewardUpdate)) if validator is not the leader and its reward can be paid now.
    /// Returns Ok(None) if validator is the leader and we haven't finished calculating its rewards.
    fn calculate_reward(&self, validator: Pubkey) -> Result<(u64, u64), Error> {
        let (reward_slot_validator_stake, _) =
            self.accounts
                .get(&validator)
                .ok_or(Error::MissingRewardSlotValidator {
                    pubkey: validator,
                    reward_slot: self.reward_slot,
                    current_slot: self.current_slot,
                })?;
        Ok(calculate_reward(
            &self.epoch_inflation_state,
            self.total_stake,
            *reward_slot_validator_stake,
        ))
    }

    fn update_account(&self, reward: u64, handler: &mut VoteStateHandler) {
        handler.increment_credits(self.reward_epoch, reward);
        let latest_root_or_reward = handler
            .root_slot()
            .unwrap_or(self.reward_slot)
            .max(self.reward_slot);
        handler.set_votes(VecDeque::from([LandedVote {
            lockout: Lockout::new(latest_root_or_reward),
            latency: 0,
        }]));
    }
}

struct FinalCertState<'a> {
    signers: &'a HashSet<Pubkey>,
    final_slot: Slot,
}

impl<'a> FinalCertState<'a> {
    fn update_account(&self, vote_pubkey: Pubkey, handler: &mut VoteStateHandler) {
        if self.signers.contains(&vote_pubkey) {
            handler.set_root_slot(Some(self.final_slot));
        }
    }
}

enum StateUpdateAccountResult {
    Err(UpdateAccountError),
    FinalCert(AccountSharedData),
    NonLeaderReward(AccountSharedData, u64),
    LeaderReward(u64),
}

enum State<'a> {
    Reward(RewardState<'a>),
    FinalCert(FinalCertState<'a>),
    Both(RewardState<'a>, FinalCertState<'a>),
}

impl<'a> State<'a> {
    fn new(
        bank: &'a Bank,
        reward_slot: Option<Slot>,
        final_cert_input: Option<(&'a HashSet<Pubkey>, Slot)>,
    ) -> Result<Option<Self>, Error> {
        match (reward_slot, final_cert_input) {
            (None, None) => Ok(None),
            (None, Some((signers, final_slot))) => Ok(Some(Self::FinalCert(FinalCertState {
                signers,
                final_slot,
            }))),
            (Some(reward_slot), None) => {
                Ok(Some(Self::Reward(RewardState::new(bank, reward_slot)?)))
            }
            (Some(reward_slot), Some((signers, final_slot))) => {
                let reward_state = RewardState::new(bank, reward_slot)?;
                let final_cert_state = FinalCertState {
                    signers,
                    final_slot,
                };
                Ok(Some(Self::Both(reward_state, final_cert_state)))
            }
        }
    }

    fn update_account(
        &self,
        vote_accounts: &HashMap<Pubkey, (u64, VoteAccount)>,
        vote_pubkey: Pubkey,
    ) -> StateUpdateAccountResult {
        match self {
            Self::Reward(state) => {
                let (validator_reward, leader_reward) =
                    state.calculate_reward(vote_pubkey).unwrap();
                if vote_pubkey == state.leader_vote_pubkey {
                    return StateUpdateAccountResult::LeaderReward(
                        validator_reward + leader_reward,
                    );
                }
                let mut my_handler =
                    MyVoteStateHandler::try_new(vote_accounts, vote_pubkey).unwrap();
                state.update_account(validator_reward, &mut my_handler.handler);
                let updated_account = my_handler.serialize().unwrap();
                StateUpdateAccountResult::NonLeaderReward(updated_account, leader_reward)
            }
            Self::FinalCert(state) => {
                let mut my_handler =
                    MyVoteStateHandler::try_new(vote_accounts, vote_pubkey).unwrap();
                state.update_account(vote_pubkey, &mut my_handler.handler);
                let updated_account = my_handler.serialize().unwrap();
                StateUpdateAccountResult::FinalCert(updated_account)
            }
            Self::Both(reward_state, final_cert_state) => {
                let (validator_reward, leader_reward) =
                    reward_state.calculate_reward(vote_pubkey).unwrap();
                if vote_pubkey == reward_state.leader_vote_pubkey {
                    return StateUpdateAccountResult::LeaderReward(
                        validator_reward + leader_reward,
                    );
                }
                let mut my_handler =
                    MyVoteStateHandler::try_new(vote_accounts, vote_pubkey).unwrap();
                reward_state.update_account(validator_reward, &mut my_handler.handler);
                final_cert_state.update_account(vote_pubkey, &mut my_handler.handler);
                let updated_account = my_handler.serialize().unwrap();
                StateUpdateAccountResult::NonLeaderReward(updated_account, leader_reward)
            }
        }
    }

    fn update_accounts(
        &self,
        vote_accounts: &HashMap<Pubkey, (u64, VoteAccount)>,
        validators: impl Iterator<Item = &'a Pubkey>,
        updated_accounts: &mut Vec<(Pubkey, AccountSharedData)>,
        total_leader_reward: &mut u64,
    ) {
        for &validator in validators {
            match self.update_account(vote_accounts, validator) {
                StateUpdateAccountResult::Err(err) => panic!("{err}"),
                StateUpdateAccountResult::FinalCert(acct) => {
                    updated_accounts.push((validator, acct))
                }
                StateUpdateAccountResult::NonLeaderReward(acct, leader_reward) => {
                    updated_accounts.push((validator, acct));
                    *total_leader_reward = total_leader_reward.saturating_add(leader_reward);
                }
                StateUpdateAccountResult::LeaderReward(leader_reward) => {
                    *total_leader_reward = total_leader_reward.saturating_add(leader_reward);
                }
            }
        }
    }

    fn update_leader(
        &self,
        vote_accounts: &HashMap<Pubkey, (u64, VoteAccount)>,
        reward: u64,
    ) -> AccountSharedData {
        match self {
            Self::FinalCert(_) => panic!(),
            Self::Reward(state) => {
                let mut my_handler =
                    MyVoteStateHandler::try_new(vote_accounts, state.leader_vote_pubkey).unwrap();
                state.update_account(reward, &mut my_handler.handler);
                my_handler.serialize().unwrap()
            }
            Self::Both(reward_state, final_cert_state) => {
                let mut my_handler =
                    MyVoteStateHandler::try_new(vote_accounts, reward_state.leader_vote_pubkey)
                        .unwrap();
                reward_state.update_account(reward, &mut my_handler.handler);
                final_cert_state
                    .update_account(reward_state.leader_vote_pubkey, &mut my_handler.handler);
                my_handler.serialize().unwrap()
            }
        }
    }
}

#[derive(Debug, Error)]
enum UpdateAccountError {
    #[error("could not find the vote account")]
    AccountNotFound,
    #[error("deserializing vote account failed with {0}")]
    DeserializeFailed(BincodeError),
    #[error("converting account into VoteStateHandler failed with {0}")]
    HandlerConversionFailed(InstructionError),
    #[error("serializing VoteStateHandler failed with {0}")]
    SerializeFailed(InstructionError),
}

fn allocate_updated_accounts(
    bank: &Bank,
    reward_cert: &Option<ValidatedRewardCert>,
    final_cert_input: &Option<(&HashSet<Pubkey>, Slot)>,
) -> Result<Option<Vec<(Pubkey, AccountSharedData)>>, Error> {
    let max_validators = match (&reward_cert, &final_cert_input) {
        (None, None) => return Ok(None),
        (Some(cert), None) => cert.validators().len(),
        (None, Some((signers, _))) => signers.len(),
        (Some(reward_cert), Some((_, slot))) => {
            let final_cert_slot_max_validators = bank
                .get_rank_map(*slot)
                .ok_or(Error::RankMapNotFound)?
                .len();
            let reward_cert_slot_max_validators = bank
                .get_rank_map(reward_cert.slot())
                .ok_or(Error::RankMapNotFound)?
                .len();
            final_cert_slot_max_validators.max(reward_cert_slot_max_validators)
        }
    };
    Ok(Some(Vec::with_capacity(max_validators)))
}

/// Calculates voting rewards based on the `reward_cert` and updates fields in the vote account
/// based on the calculated rewards and the `final_cert`.
pub(super) fn calc_vote_rewards_update_vote_states(
    bank: &Bank,
    reward_cert: Option<ValidatedRewardCert>,
    final_cert_input: Option<(&HashSet<Pubkey>, Slot)>,
) -> Result<(), Error> {
    let Some(mut updated_accounts) =
        allocate_updated_accounts(bank, &reward_cert, &final_cert_input)?
    else {
        return Ok(());
    };

    let vote_accounts = bank.vote_accounts();

    match (reward_cert, &final_cert_input) {
        (None, None) => Ok(()),

        (None, Some((signers, _))) => {
            let mut total_leader_reward = 0;
            let state = State::new(bank, None, final_cert_input).unwrap().unwrap();
            state.update_accounts(
                &vote_accounts,
                signers.iter(),
                &mut updated_accounts,
                &mut total_leader_reward,
            );
            assert_eq!(total_leader_reward, 0);
            bank.store_accounts((bank.slot(), updated_accounts.as_slice()));
            Ok(())
        }

        (Some(reward_cert), None) => {
            let (reward_slot, reward_validators) = reward_cert.into_parts();
            let mut total_leader_reward = 0;
            let state = State::new(bank, Some(reward_slot), None).unwrap().unwrap();
            state.update_accounts(
                &vote_accounts,
                reward_validators.iter(),
                &mut updated_accounts,
                &mut total_leader_reward,
            );
            let leader_vote_pubkey = bank.leader().vote_address;
            let leader_account = state.update_leader(&vote_accounts, total_leader_reward);
            updated_accounts.push((leader_vote_pubkey, leader_account));
            bank.store_accounts((bank.slot(), updated_accounts.as_slice()));
            Ok(())
        }

        (Some(reward_cert), Some((signers, _))) => {
            let (reward_slot, reward_validators) = reward_cert.into_parts();
            let mut total_leader_reward = 0;
            let state = State::new(bank, None, final_cert_input).unwrap().unwrap();
            state.update_accounts(
                &vote_accounts,
                reward_validators.difference(signers),
                &mut updated_accounts,
                &mut total_leader_reward,
            );
            assert_eq!(total_leader_reward, 0);

            let state = State::new(bank, Some(reward_slot), final_cert_input)
                .unwrap()
                .unwrap();
            state.update_accounts(
                &vote_accounts,
                reward_validators.intersection(signers),
                &mut updated_accounts,
                &mut total_leader_reward,
            );

            let state = State::new(bank, Some(reward_slot), None).unwrap().unwrap();
            state.update_accounts(
                &vote_accounts,
                signers.difference(&reward_validators),
                &mut updated_accounts,
                &mut total_leader_reward,
            );

            let leader_vote_pubkey = bank.leader().vote_address;
            let leader_account = state.update_leader(&vote_accounts, total_leader_reward);
            updated_accounts.push((leader_vote_pubkey, leader_account));
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
            bank_forks::BankForks,
            genesis_utils::{
                ValidatorVoteKeypairs, create_genesis_config_with_alpenglow_vote_accounts,
            },
            validated_block_finalization::ValidatedBlockFinalizationCert,
        },
        agave_votor_messages::{
            consensus_message::{Certificate, CertificateType},
            reward_certificate::NUM_SLOTS_FOR_REWARD,
        },
        bitvec::prelude::*,
        rand::seq::IndexedRandom,
        solana_account::ReadableAccount,
        solana_bls_signatures::{BLS_SIGNATURE_AFFINE_SIZE, Signature as BLSSignature},
        solana_epoch_schedule::EpochSchedule,
        solana_genesis_config::GenesisConfig,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_leader_schedule::SlotLeader,
        solana_native_token::LAMPORTS_PER_SOL,
        solana_rent::Rent,
        solana_signer::Signer,
        solana_signer_store::encode_base2,
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
}
