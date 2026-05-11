use {
    crate::{bank::Bank, validated_reward_certificate::ValidatedRewardCert},
    epoch_inflation_account_state::{EpochInflationAccountState, EpochInflationState},
    log::info,
    solana_account::{AccountSharedData, ReadableAccount, WritableAccount},
    solana_clock::{Epoch, Slot},
    solana_pubkey::Pubkey,
    solana_vote::vote_account::VoteAccount,
    solana_vote_interface::state::{LandedVote, Lockout},
    solana_vote_program::vote_state::handler::VoteStateHandler,
    std::collections::{HashMap, HashSet, VecDeque, hash_map::Entry},
    thiserror::Error,
};

pub mod epoch_inflation_account_state;

/// Different types of errors that can happen when calculating and paying voting reward.
///
/// These errors should cause the processing of the bank to fail.
#[derive(Debug, Error)]
pub enum CalcVoteRewardUpdateVoteStatesError {
    #[error("allocating accounts failed with {0}")]
    AllocateAccounts(#[from] AllocateAccountsError),
    #[error("Processing reward state failed with {0}")]
    RewardState(#[from] RewardStateError),
}

/// Different types of error that happen when allocating memory for storing updated accounts.
///
/// These errors should cause the processing of the bank to fail.
#[derive(Debug, Error)]
pub enum AllocateAccountsError {
    #[error("did not find rank map for final_slot={final_slot} in current_slot={current_slot}")]
    FinalCert {
        current_slot: Slot,
        final_slot: Slot,
    },
    #[error("did not find rank map for reward_slot={reward_slot} in current_slot={current_slot}")]
    RewardCert {
        current_slot: Slot,
        reward_slot: Slot,
    },
}

/// Different types of error that happen when looking up state to process the reward cert.
///
/// These errors should cause the processing of the bank to fail.
#[derive(Debug, Error)]
pub enum RewardStateError {
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

/// Data needed to operate on `VoteStateHandler`.
struct VoteState {
    /// The pubkey of the vote account.
    vote_pubkey: Pubkey,
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
    ) -> Option<Self> {
        let Some((_, account)) = vote_accounts.get(&vote_pubkey) else {
            info!("did not find vote account for vote_pubkey={vote_pubkey}");
            return None;
        };
        let versions = match bincode::deserialize(account.account().data()) {
            Ok(s) => s,
            Err(e) => {
                info!("bincode::deserialize for vote_pubkey={vote_pubkey} failed with {e}");
                return None;
            }
        };
        let handler = match VoteStateHandler::try_new_from_vote_state_versions(versions) {
            Ok(h) => h,
            Err(e) => {
                info!("VoteStateHandler::try_new() for vote_pubkey={vote_pubkey} failed with {e}");
                return None;
            }
        };
        Some(Self {
            vote_pubkey,
            handler,
            lamports: account.lamports(),
            space: account.account().data().len(),
            owner: *account.owner(),
        })
    }

    fn serialize(self) -> Option<(Pubkey, AccountSharedData)> {
        let mut updated_account = AccountSharedData::new(self.lamports, self.space, &self.owner);
        match self
            .handler
            .serialize_into(updated_account.data_as_mut_slice())
        {
            Ok(()) => Some((self.vote_pubkey, updated_account)),
            Err(e) => {
                info!(
                    "serializing account vote_pubkey={} failed with {e}",
                    self.vote_pubkey
                );
                None
            }
        }
    }
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
    fn try_new(
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
        accumulating_leader_reward: &mut u64,
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
        *accumulating_leader_reward = accumulating_leader_reward.saturating_add(leader_reward);
        Ok(validator_reward)
    }

    fn update_votes(&self, vote_state: &mut VoteState) {
        debug_assert!(self.reward_validators.contains(&vote_state.vote_pubkey));
        let latest_voted_slot = self
            .reward_slot
            .max(vote_state.handler.last_voted_slot().unwrap_or(0))
            .max(vote_state.handler.root_slot().unwrap_or(0));
        vote_state.handler.set_votes(VecDeque::from([LandedVote {
            lockout: Lockout::new(latest_voted_slot),
            latency: 0,
        }]));
    }

    fn update_account(
        &self,
        vote_state: &mut VoteState,
        accumulating_leader_reward: &mut u64,
    ) -> Result<bool, RewardStateError> {
        if self.reward_validators.contains(&vote_state.vote_pubkey) {
            self.update_votes(vote_state);
            let reward =
                self.calculate_reward(vote_state.vote_pubkey, accumulating_leader_reward)?;
            if reward != 0 {
                vote_state
                    .handler
                    .increment_credits(self.current_epoch, reward);
            }
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Assumes that `Self::update_account` was already called and pays the additional
    /// `leader_rewards` to the account.
    #[must_use]
    fn update_leader(&self, vote_state: &mut VoteState, leader_reward: u64) -> bool {
        debug_assert_eq!(vote_state.vote_pubkey, self.leader_vote_pubkey);
        if leader_reward != 0 {
            vote_state
                .handler
                .increment_credits(self.current_epoch, leader_reward);
            true
        } else {
            false
        }
    }
}

/// Common state required to update root_slot on the vote account.
#[derive(Debug)]
struct FinalCertState<'a> {
    signers: &'a HashSet<Pubkey>,
    final_slot: Slot,
}

impl<'a> FinalCertState<'a> {
    /// Updates the `root_slot` and the `votes` fields in the `VoteStateHandler`.
    #[must_use]
    fn update_account(&self, vote_state: &mut VoteState) -> bool {
        if self.signers.contains(&vote_state.vote_pubkey) {
            vote_state.handler.set_root_slot(Some(self.final_slot));
            let latest_voted_slot = vote_state
                .handler
                .last_voted_slot()
                .unwrap_or(self.final_slot)
                .max(self.final_slot);
            // If a validator is included in the finalization cert, it must have voted for it.
            // So even if the reward cert is absent, we can still update votes.
            vote_state.handler.set_votes(VecDeque::from([LandedVote {
                lockout: Lockout::new(latest_voted_slot),
                latency: 0,
            }]));
            true
        } else {
            false
        }
    }
}

/// Allocates storage for updated accounts.
fn allocate_updated_accounts(
    bank: &Bank,
    reward_cert: &Option<ValidatedRewardCert>,
    final_cert_input: &Option<(&HashSet<Pubkey>, Slot)>,
) -> Result<Option<HashMap<Pubkey, VoteState>>, AllocateAccountsError> {
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
                .ok_or(AllocateAccountsError::FinalCert {
                    current_slot: bank.slot(),
                    final_slot: *slot,
                })?
                .len();
            let reward_cert_slot_max_validators = bank
                .get_rank_map(reward_cert.slot())
                .ok_or(AllocateAccountsError::RewardCert {
                    current_slot: bank.slot(),
                    reward_slot: reward_cert.slot(),
                })?
                .len();
            final_cert_slot_max_validators.max(reward_cert_slot_max_validators)
        }
    };
    Ok(Some(HashMap::with_capacity(max_validators)))
}

fn update_accounts(
    reward_state: &Option<RewardState>,
    final_cert_state: &Option<FinalCertState>,
    vote_accounts: &HashMap<Pubkey, (u64, VoteAccount)>,
    mut updated_accounts: HashMap<Pubkey, VoteState>,
    validators: impl Iterator<Item = Pubkey>,
) -> Result<Vec<(Pubkey, AccountSharedData)>, CalcVoteRewardUpdateVoteStatesError> {
    let mut leader_reward = 0;
    for validator in validators {
        let Some(mut vote_state) = VoteState::try_new(vote_accounts, validator) else {
            continue;
        };
        let account_updated = match (reward_state, final_cert_state) {
            (None, None) => false,
            (Some(state), None) => state.update_account(&mut vote_state, &mut leader_reward)?,
            (None, Some(state)) => state.update_account(&mut vote_state),
            (Some(reward_state), Some(final_state)) => {
                let reward_updated =
                    reward_state.update_account(&mut vote_state, &mut leader_reward)?;
                let final_cert_updated = final_state.update_account(&mut vote_state);
                reward_updated || final_cert_updated
            }
        };
        if account_updated {
            updated_accounts.insert(vote_state.vote_pubkey, vote_state);
        }
    }

    // all validators have been processed, can pay leader rewards now.
    if let Some(state) = &reward_state {
        match updated_accounts.entry(state.leader_vote_pubkey) {
            Entry::Occupied(e) => {
                let _ = state.update_leader(e.into_mut(), leader_reward);
            }
            Entry::Vacant(e) => {
                if let Some(mut vote_state) =
                    VoteState::try_new(vote_accounts, state.leader_vote_pubkey)
                {
                    if state.update_leader(&mut vote_state, leader_reward) {
                        e.insert(vote_state);
                    }
                }
            }
        }
    }

    Ok(updated_accounts
        .into_values()
        .filter_map(|vote_state| vote_state.serialize())
        .collect())
}

/// Calculates voting rewards based on the `reward_cert` and updates fields in the vote account
/// based on the calculated rewards and the `final_cert_input`.
pub(super) fn calc_vote_rewards_update_vote_states(
    bank: &Bank,
    reward_cert: Option<ValidatedRewardCert>,
    final_cert_input: Option<(&HashSet<Pubkey>, Slot)>,
) -> Result<(), CalcVoteRewardUpdateVoteStatesError> {
    let Some(updated_accounts) = allocate_updated_accounts(bank, &reward_cert, &final_cert_input)?
    else {
        return Ok(());
    };
    let reward_state = match &reward_cert {
        Some(c) => Some(RewardState::try_new(bank, c.slot(), c.validators())?),
        None => None,
    };
    let final_cert_state = final_cert_input.map(|(signers, final_slot)| FinalCertState {
        signers,
        final_slot,
    });
    let vote_accounts = bank.vote_accounts();

    let updated_accounts = match (&reward_state, &final_cert_state) {
        (None, None) => return Ok(()),
        (Some(state), None) => update_accounts(
            &reward_state,
            &final_cert_state,
            &vote_accounts,
            updated_accounts,
            state.reward_validators.iter().cloned(),
        )?,
        (None, Some(state)) => update_accounts(
            &reward_state,
            &final_cert_state,
            &vote_accounts,
            updated_accounts,
            state.signers.iter().cloned(),
        )?,
        (Some(r_state), Some(f_state)) => update_accounts(
            &reward_state,
            &final_cert_state,
            &vote_accounts,
            updated_accounts,
            r_state.reward_validators.union(f_state.signers).cloned(),
        )?,
    };

    bank.store_accounts((bank.slot(), updated_accounts.as_slice()));
    Ok(())
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
        std::sync::{Arc, RwLock},
    };

    fn new_bank_for_tests(
        leader: SlotLeader,
        genesis_config: &GenesisConfig,
    ) -> (Arc<Bank>, Arc<RwLock<BankForks>>) {
        let bank = Bank::new_with_paths_for_tests(genesis_config, None, vec![], Some(leader));
        assert_eq!(*bank.leader(), leader);
        bank.wrap_with_bank_forks_for_tests()
    }

    fn new_bank_from_parent(parent_bank: Arc<Bank>, slot: Slot) -> Arc<Bank> {
        let leader = *parent_bank.leader();
        Arc::new(Bank::new_from_parent(parent_bank, leader, slot))
    }

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

        let (bank, _bank_forks) =
            new_bank_for_tests(SlotLeader::new_unique(), &GenesisConfig::default());
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

        let (prev_bank, _bank_forks) = new_bank_for_tests(slot_leader, &genesis_config);
        let current_slot = prev_bank
            .epoch_schedule
            .get_first_slot_in_epoch(prev_bank.epoch() + 1)
            + NUM_SLOTS_FOR_REWARD;
        let bank = new_bank_from_parent(prev_bank.clone(), current_slot);
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

        let (prev_bank, _bank_forks) = new_bank_for_tests(slot_leader, &genesis_config);
        let current_slot = prev_bank
            .epoch_schedule
            .get_first_slot_in_epoch(prev_bank.epoch() + 1)
            + NUM_SLOTS_FOR_REWARD;
        let bank = new_bank_from_parent(prev_bank.clone(), current_slot);
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

        let (prev_bank, _bank_forks) = new_bank_for_tests(slot_leader, &genesis_config);
        let current_slot = prev_bank
            .epoch_schedule
            .get_first_slot_in_epoch(prev_bank.epoch() + 1)
            + NUM_SLOTS_FOR_REWARD;
        let bank = new_bank_from_parent(prev_bank.clone(), current_slot);
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

        let node_pubkey = node_keypair.pubkey();
        let vote_pubkey = validator_keypairs[0].vote_keypair.pubkey();
        let slot_leader = SlotLeader {
            id: node_pubkey,
            vote_address: vote_pubkey,
        };

        let (prev_bank, _bank_forks) = new_bank_for_tests(slot_leader, &genesis_config);
        let current_slot = prev_bank
            .epoch_schedule
            .get_first_slot_in_epoch(prev_bank.epoch() + 1)
            + NUM_SLOTS_FOR_REWARD;

        let bank = new_bank_from_parent(prev_bank.clone(), current_slot);
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
