use {
    crate::{bank::Bank, validated_reward_certificate::ValidatedRewardCert},
    agave_votor_messages::migration::AG_MIGRATION_EPOCH_CREDIT,
    epoch_inflation_account_state::{EpochInflationAccountState, EpochInflationState},
    log::info,
    solana_account::{AccountSharedData, ReadableAccount, WritableAccount},
    solana_clock::{Epoch, Slot},
    solana_pubkey::Pubkey,
    solana_vote::vote_account::VoteAccount,
    solana_vote_interface::state::{
        BlockTimestamp, LandedVote, Lockout, MAX_EPOCH_CREDITS_HISTORY,
    },
    solana_vote_program::vote_state::handler::VoteStateHandler,
    std::{
        collections::{HashMap, HashSet, VecDeque, hash_map::Entry},
        num::NonZero,
    },
    thiserror::Error,
};

pub(crate) mod epoch_inflation_account_state;
mod migration_test;

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
    #[error("did not find rank map for final_slot={final_slot} in bank_slot={bank_slot}")]
    FinalCert { bank_slot: Slot, final_slot: Slot },
    #[error("did not find rank map for reward_slot={reward_slot} in bank_slot={bank_slot}")]
    RewardCert { bank_slot: Slot, reward_slot: Slot },
}

/// Different types of error that happen when looking up state to process the reward cert.
///
/// These errors should cause the processing of the bank to fail.
#[derive(Debug, Error)]
pub enum RewardStateError {
    #[error("missing epoch stakes for reward_slot {reward_slot} in bank_slot {bank_slot}")]
    MissingEpochStakes { reward_slot: Slot, bank_slot: Slot },
    #[error("missing EpochInflationAccountState for bank_slot {bank_slot}")]
    MissingEpochInflationAccountState { bank_slot: Slot },
    #[error(
        "missing validator stake info for reward epoch {reward_epoch} in bank_slot {bank_slot}"
    )]
    NoEpochValidatorStake {
        reward_epoch: Epoch,
        bank_slot: Slot,
    },
    #[error("validator {pubkey} missing in bank_slot {bank_slot} for reward slot {reward_slot}")]
    MissingRewardSlotValidator {
        pubkey: Pubkey,
        reward_slot: Slot,
        bank_slot: Slot,
    },
    #[error("genesis cert not found. reward_slot={reward_slot}; bank_slot={bank_slot}")]
    GenesisCertNotFound { reward_slot: Slot, bank_slot: Slot },
}

/// Data needed to operate on `VoteStateHandler`.
#[derive(Debug)]
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

    /// Updates `votes` and `last_timestamp` in the vote state.
    fn maybe_update_votes(&mut self, slot: Slot, slot_timestamp_ns: i64) {
        let latest_voted_slot = slot
            .max(self.handler.last_voted_slot().unwrap_or(0))
            .max(self.handler.root_slot().unwrap_or(0));
        self.handler.set_votes(VecDeque::from([LandedVote {
            lockout: Lockout::new(latest_voted_slot),
            latency: 0,
        }]));

        let timestamp = slot_timestamp_ns / 1_000_000_000;
        if timestamp > self.handler.last_timestamp().timestamp {
            self.handler
                .set_last_timestamp(BlockTimestamp { slot, timestamp });
        }
    }

    /// If `slot` is bigger than vote_state.root_slot, then updates vote_state.root_slot.
    fn maybe_update_root(&mut self, slot: Slot) {
        let latest_root = self.handler.root_slot().unwrap_or(slot).max(slot);
        self.handler.set_root_slot(Some(latest_root));
    }
}

/// Common state required to pay rewards.
#[derive(Debug)]
struct RewardState<'a> {
    reward_slot_timestamp_ns: i64,
    /// The epoch in which the reward was paid into the vote account.
    current_epoch: Epoch,
    /// The slot in which the reward was earned.
    reward_slot: Slot,
    /// Validators present in the reward cert.
    reward_validators: &'a HashSet<Pubkey>,
    /// The slot in which the reward is being paid into the vote account.
    bank_slot: Slot,
    /// The pubkey of the validator that will receive the leader rewards.
    leader_vote_pubkey: Pubkey,
    /// Vote accounts at reward slot.
    accounts: &'a HashMap<Pubkey, (u64, VoteAccount)>,
    /// Total stake at `reward_slot`.
    total_stake: u64,
    /// inflation state at `reward_slot`.
    epoch_inflation_state: EpochInflationState,
    migration_epoch: Epoch,
}

impl<'a> RewardState<'a> {
    fn try_new(
        bank: &'a Bank,
        reward_slot: Slot,
        reward_validators: &'a HashSet<Pubkey>,
        block_producer_time_nanos: i64,
    ) -> Result<Self, RewardStateError> {
        let bank_slot = bank.slot();
        let epoch_stakes = bank.epoch_stakes_from_slot(reward_slot).ok_or(
            RewardStateError::MissingEpochStakes {
                reward_slot,
                bank_slot,
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
                .ok_or(RewardStateError::MissingEpochInflationAccountState { bank_slot })?
                .get_epoch_state(reward_epoch)
                .ok_or(RewardStateError::NoEpochValidatorStake {
                    reward_epoch,
                    bank_slot,
                })?
        };
        let migration_epoch =
            get_migration_epoch(bank).ok_or(RewardStateError::GenesisCertNotFound {
                reward_slot,
                bank_slot,
            })?;
        let reward_slot_timestamp_ns =
            calc_slot_timestamp(bank, reward_slot, block_producer_time_nanos);
        Ok(Self {
            reward_slot_timestamp_ns,
            current_epoch: bank.epoch(),
            reward_slot,
            reward_validators,
            bank_slot,
            leader_vote_pubkey: bank.leader().vote_address,
            accounts,
            total_stake,
            epoch_inflation_state,
            migration_epoch,
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
                    bank_slot: self.bank_slot,
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
        vote_state.maybe_update_votes(self.reward_slot, self.reward_slot_timestamp_ns);
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
            if let Some(reward) = NonZero::new(reward) {
                increment_credits(
                    vote_state.handler.epoch_credits_mut(),
                    self.migration_epoch,
                    self.current_epoch,
                    reward,
                );
            };
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Assumes that `Self::update_account` was already called and pays the additional
    /// `leader_rewards` to the account.
    fn update_leader(&self, vote_state: &mut VoteState, leader_reward: NonZero<u64>) {
        debug_assert_eq!(vote_state.vote_pubkey, self.leader_vote_pubkey);
        increment_credits(
            vote_state.handler.epoch_credits_mut(),
            self.migration_epoch,
            self.current_epoch,
            leader_reward,
        );
    }
}

/// Common state required to update root_slot on the vote account.
#[derive(Debug)]
struct FinalCertState<'a> {
    signers: &'a HashSet<Pubkey>,
    final_slot: Slot,
    final_slot_timestamp_ns: i64,
}

impl<'a> FinalCertState<'a> {
    fn new(
        bank: &Bank,
        signers: &'a HashSet<Pubkey>,
        final_slot: Slot,
        block_producer_time_nanos: i64,
    ) -> Self {
        let final_slot_timestamp_ns =
            calc_slot_timestamp(bank, final_slot, block_producer_time_nanos);
        Self {
            signers,
            final_slot,
            final_slot_timestamp_ns,
        }
    }

    /// Updates the `root_slot` and the `votes` fields in the `VoteStateHandler`.
    #[must_use]
    fn update_account(&self, vote_state: &mut VoteState) -> bool {
        if self.signers.contains(&vote_state.vote_pubkey) {
            vote_state.maybe_update_root(self.final_slot);
            // If a validator is included in the finalization cert, it must have voted for it.
            // So even if the reward cert is absent, we can still update votes.
            vote_state.maybe_update_votes(self.final_slot, self.final_slot_timestamp_ns);
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
                    bank_slot: bank.slot(),
                    final_slot: *slot,
                })?
                .len();
            let reward_cert_slot_max_validators = bank
                .get_rank_map(reward_cert.slot())
                .ok_or(AllocateAccountsError::RewardCert {
                    bank_slot: bank.slot(),
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
    if let Some(state) = &reward_state
        && let Some(leader_reward) = NonZero::new(leader_reward)
    {
        match updated_accounts.entry(state.leader_vote_pubkey) {
            Entry::Occupied(e) => {
                state.update_leader(e.into_mut(), leader_reward);
            }
            Entry::Vacant(e) => {
                if let Some(mut vote_state) =
                    VoteState::try_new(vote_accounts, state.leader_vote_pubkey)
                {
                    state.update_leader(&mut vote_state, leader_reward);
                    e.insert(vote_state);
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
    block_producer_time_nanos: i64,
) -> Result<(), CalcVoteRewardUpdateVoteStatesError> {
    let Some(updated_accounts) = allocate_updated_accounts(bank, &reward_cert, &final_cert_input)?
    else {
        return Ok(());
    };
    let reward_state = match &reward_cert {
        Some(c) => Some(RewardState::try_new(
            bank,
            c.slot(),
            c.validators(),
            block_producer_time_nanos,
        )?),
        None => None,
    };
    let final_cert_state = final_cert_input.map(|(signers, final_slot)| {
        FinalCertState::new(bank, signers, final_slot, block_producer_time_nanos)
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

fn calc_slot_timestamp(bank: &Bank, slot: Slot, block_producer_time_nanos: i64) -> i64 {
    block_producer_time_nanos.saturating_sub(
        i64::try_from(bank.slot_range_duration_nanos(slot + 1, bank.slot())).unwrap(),
    )
}

fn get_migration_epoch(bank: &Bank) -> Option<Epoch> {
    let migration_slot = bank.get_alpenglow_migration_slot()?;
    let migration_epoch = bank.epoch_schedule.get_epoch(migration_slot);
    Some(migration_epoch)
}

fn ensure_marker(epoch_credits: &mut Vec<(Epoch, u64, u64)>) {
    for elem in epoch_credits.iter().rev() {
        if elem == &AG_MIGRATION_EPOCH_CREDIT {
            return;
        }
    }
    epoch_credits.push(AG_MIGRATION_EPOCH_CREDIT);
}

fn increment_credits(
    epoch_credits: &mut Vec<(Epoch, u64, u64)>,
    migration_epoch: Epoch,
    epoch: Epoch,
    new_credits: NonZero<u64>,
) {
    if epoch == migration_epoch {
        ensure_marker(epoch_credits);
    }

    let Some(entry) = epoch_credits.last_mut() else {
        // no entries, insert a new entry and we are done.
        epoch_credits.push((epoch, new_credits.get(), 0));
        return;
    };

    // Latest element is the marker, start a new entry.
    if *entry == AG_MIGRATION_EPOCH_CREDIT {
        // If there was a tower entry before, its final credits forms this entry's initial credits.
        let len = epoch_credits.len();
        let final_tower_credits = if len >= 2 {
            assert_ne!(epoch_credits[len - 2], AG_MIGRATION_EPOCH_CREDIT);
            epoch_credits[len - 2].1
        } else {
            0
        };
        epoch_credits.push((
            epoch,
            new_credits.get().saturating_add(final_tower_credits),
            final_tower_credits,
        ));
        while epoch_credits.len() > MAX_EPOCH_CREDITS_HISTORY {
            epoch_credits.remove(0);
        }
        return;
    }

    let (entry_epoch, final_credits, initial_credits) = entry;

    // Latest element is the same epoch, simply increment final credits.
    if *entry_epoch == epoch {
        *final_credits = final_credits.saturating_add(new_credits.get());
        return;
    }

    // Different epochs but the latest epoch didn't earn any credits, reuse the entry.
    if final_credits == initial_credits {
        *entry_epoch = epoch;
        *final_credits = final_credits.saturating_add(new_credits.get());
        return;
    }

    // Different epochs and the latest epoch earned credits, insert a new entry.
    let entry = (
        epoch,
        new_credits.get().saturating_add(*final_credits),
        *final_credits,
    );
    epoch_credits.push(entry);

    // maybe included a marker and a new entry above.  So might have multiple entries to remove here.
    while epoch_credits.len() > MAX_EPOCH_CREDITS_HISTORY {
        epoch_credits.remove(0);
    }
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
            certificate::{Certificate, CertificateType},
            consensus_message::Block,
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
            time::{SystemTime, UNIX_EPOCH},
        },
        test_case::test_matrix,
    };

    fn new_bank_for_tests(
        leader: SlotLeader,
        genesis_config: &GenesisConfig,
    ) -> (Arc<Bank>, Arc<RwLock<BankForks>>) {
        let bank = Bank::new_with_paths_for_tests(genesis_config, None, vec![], Some(leader));
        assert_eq!(*bank.leader(), leader);
        bank.wrap_with_bank_forks_for_tests()
    }

    pub fn new_bank_from_parent(parent_bank: Arc<Bank>, slot: Slot) -> Arc<Bank> {
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
        let block = Block {
            slot: bank.slot(),
            block_id: Hash::new_unique(),
        };
        let cert_type = CertificateType::FinalizeFast(block);
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
        bank: &Bank,
        epoch_start_capitalization: u64,
        reward_epoch: Epoch,
        total_stake: u64,
        stake_voted: u64,
    ) -> (u64, u64) {
        let epoch_inflation =
            bank.calculate_epoch_inflation_rewards(epoch_start_capitalization, reward_epoch);
        let numerator = epoch_inflation as u128 * stake_voted as u128;
        let denominator = bank.epoch_schedule.slots_per_epoch as u128 * total_stake as u128;
        let reward: u64 = (numerator / denominator).try_into().unwrap();
        let validator_reward = reward / 2;
        let leader_reward = reward - validator_reward;
        (validator_reward, leader_reward)
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

        let block_producer_time_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i64;
        calc_vote_rewards_update_vote_states(
            &bank,
            Some(ValidatedRewardCert::new_for_tests(
                reward_slot,
                validator_pubkeys_to_reward.clone(),
            )),
            None,
            block_producer_time_nanos,
        )
        .unwrap();
        let slot_timestamp = BlockTimestamp {
            slot: reward_slot,
            timestamp: calc_slot_timestamp(&bank, reward_slot, block_producer_time_nanos)
                / 1_000_000_000,
        };

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
                let reward_epoch = bank.epoch_schedule.get_epoch(reward_slot);
                assert_eq!(vote_state.last_timestamp(), &slot_timestamp);
                let (expected_validator_reward, expected_leader_reward_per_validator) =
                    calc_reward_for_test(
                        &bank,
                        prev_bank.capitalization(),
                        reward_epoch,
                        total_stake,
                        per_validator_stake,
                    );
                if *validator != leader_vote_pubkey {
                    assert_eq!(got_reward, expected_validator_reward);
                }
                (
                    got_reward,
                    expected_validator_reward,
                    expected_leader_reward_per_validator,
                )
            })
            .collect::<Vec<_>>();
        let (leader_reward, expected_validator_reward, expected_leader_reward_per_validator) =
            rewards[0];
        assert_eq!(
            expected_validator_reward
                + expected_leader_reward_per_validator * validator_pubkeys_to_reward.len() as u64,
            leader_reward
        );
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
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos() as i64,
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
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos() as i64,
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
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos() as i64,
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

    pub fn set_commission(
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

    struct State {
        commission_bps: u16,
        _bank_forks: Arc<RwLock<BankForks>>,
        validators: Vec<ValidatorVoteKeypairs>,
        /// key is validator pubkey, value is list of stakers.
        stakers: HashMap<Pubkey, Vec<Pubkey>>,
    }

    impl State {
        fn new(
            num_validators: u64,
            num_add_stakers: u64,
            pay_leader: bool,
            commission_bps: u16,
        ) -> (Self, Arc<Bank>) {
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

            let (bank_epoch0, bank_forks) = new_bank_for_tests(leader, &genesis_config);
            assert_eq!(bank_epoch0.epoch(), 0);

            // need to bump epoch by 1 as epoch 0 is migration epoch
            let first_slot_in_epoch1 = bank_epoch0
                .epoch_schedule
                .get_first_slot_in_epoch(bank_epoch0.epoch() + 1);
            let bank_epoch1 = new_bank_from_parent(bank_epoch0, first_slot_in_epoch1);
            (
                Self {
                    commission_bps,
                    _bank_forks: bank_forks,
                    validators,
                    stakers: staker_pubkeys,
                },
                bank_epoch1,
            )
        }

        fn get_validator_stake(&self, bank: &Bank, pubkey: &Pubkey) -> u64 {
            let rent_exempt_reserve = bank
                .rent_collector()
                .rent
                .minimum_balance(StakeStateV2::size_of());
            self.stakers
                .get(pubkey)
                .unwrap()
                .iter()
                .map(|pubkey| {
                    let lamports = bank.get_account(pubkey).unwrap().lamports();
                    lamports - rent_exempt_reserve
                })
                .sum()
        }

        /// returns (validator_rewards, leader_rewards)
        fn get_rewards(&self, bank: &Bank, num_reward_slots: u64, pubkey: &Pubkey) -> (u64, u64) {
            let epoch_state = EpochInflationAccountState::new_from_bank(bank)
                .unwrap()
                .get_epoch_state(bank.epoch())
                .unwrap();
            let total_stake = bank.epoch_stakes(bank.epoch()).unwrap().total_stake();
            let (validator_reward, leader_reward) = calculate_reward(
                &epoch_state,
                total_stake,
                self.get_validator_stake(bank, pubkey),
            );
            let vote_rewards = validator_reward * num_reward_slots;
            let leader_reward = leader_reward * num_reward_slots;
            (vote_rewards, leader_reward)
        }

        fn get_initial_and_final_lamports(
            &self,
            reward_bank: &Bank,
            payout_bank: &Bank,
            pubkey: &Pubkey,
        ) -> (u64, u64) {
            assert!(payout_bank.slot() > reward_bank.slot());
            assert!(payout_bank.epoch() > reward_bank.epoch());
            let initial_lamports = reward_bank.get_account(pubkey).unwrap().lamports();
            let final_lamports = payout_bank.get_account(pubkey).unwrap().lamports();
            (initial_lamports, final_lamports)
        }

        fn validate_stakers(
            &self,
            reward_bank: &Bank,
            payout_bank: &Bank,
            voter_pubkey: &Pubkey,
            validator_reward: u64,
        ) -> u64 {
            assert!(payout_bank.slot() > reward_bank.slot());
            assert!(payout_bank.epoch() > reward_bank.epoch());
            let rent_exempt_reserve = reward_bank
                .rent_collector()
                .rent
                .minimum_balance(StakeStateV2::size_of());
            let validator_stake = self.get_validator_stake(reward_bank, voter_pubkey);
            let mut expected_validator_reward = 0;
            for staker_pubkey in self.stakers.get(voter_pubkey).unwrap().iter() {
                let (initial_lamports, final_lamports) =
                    self.get_initial_and_final_lamports(reward_bank, payout_bank, staker_pubkey);
                if initial_lamports <= LAMPORTS_PER_SOL + rent_exempt_reserve {
                    continue;
                }
                let stake = initial_lamports - rent_exempt_reserve;
                let stake_weighted_reward = validator_reward * stake / validator_stake;
                let (voter_reward, staker_reward, is_split) =
                    commission_split(self.commission_bps, stake_weighted_reward);
                assert!(is_split);
                assert_eq!(
                    staker_reward,
                    final_lamports - initial_lamports,
                    "final={final_lamports}; initial={initial_lamports}"
                );
                expected_validator_reward += voter_reward;
            }
            expected_validator_reward
        }

        /// Returns leader_rewards
        fn validate_voter_reward(
            &self,
            reward_bank: &Bank,
            payout_bank: &Bank,
            num_reward_slots: u64,
            voter_pubkey: &Pubkey,
        ) -> u64 {
            assert!(payout_bank.slot() > reward_bank.slot());
            assert!(payout_bank.epoch() > reward_bank.epoch());

            let (validator_reward, leader_reward) =
                self.get_rewards(reward_bank, num_reward_slots, voter_pubkey);

            let expected_validator_reward =
                self.validate_stakers(reward_bank, payout_bank, voter_pubkey, validator_reward);

            let (initial_validator_lamports, final_validator_lamports) =
                self.get_initial_and_final_lamports(reward_bank, payout_bank, voter_pubkey);
            assert_eq!(
                expected_validator_reward,
                final_validator_lamports
                    + VAT_TO_BURN_PER_EPOCH * (payout_bank.epoch() - reward_bank.epoch())
                    - initial_validator_lamports
            );
            leader_reward
        }

        fn validate_leader_reward(
            &self,
            reward_bank: &Bank,
            payout_bank: &Bank,
            num_reward_slots: u64,
            leader: Pubkey,
            add_leader_reward: u64,
        ) {
            if !reward_bank.vote_accounts().contains_key(&leader) {
                return;
            }
            assert!(payout_bank.slot() > reward_bank.slot());
            assert!(payout_bank.epoch() > reward_bank.epoch());

            let (validator_reward, leader_reward) =
                self.get_rewards(reward_bank, num_reward_slots, &leader);
            let validator_reward = validator_reward + leader_reward + add_leader_reward;

            let expected_validator_reward =
                self.validate_stakers(reward_bank, payout_bank, &leader, validator_reward);

            let (initial_validator_lamports, final_validator_lamports) =
                self.get_initial_and_final_lamports(reward_bank, payout_bank, &leader);
            assert_eq!(
                expected_validator_reward,
                final_validator_lamports
                    + VAT_TO_BURN_PER_EPOCH * (payout_bank.epoch() - reward_bank.epoch())
                    - initial_validator_lamports
            );
        }

        fn validate_rewards(&self, reward_bank: &Bank, payout_bank: &Bank, num_reward_slots: u64) {
            assert_eq!(reward_bank.leader(), payout_bank.leader());
            let leader = reward_bank.leader().vote_address;

            let mut add_leader_reward = 0;
            for voter_pubkey in self.stakers.keys() {
                if voter_pubkey == &leader {
                    continue;
                }
                let leader_reward = self.validate_voter_reward(
                    reward_bank,
                    payout_bank,
                    num_reward_slots,
                    voter_pubkey,
                );
                add_leader_reward += leader_reward;
            }
            self.validate_leader_reward(
                reward_bank,
                payout_bank,
                num_reward_slots,
                leader,
                add_leader_reward,
            );
        }
    }

    fn reward_validators(
        bank: Arc<Bank>,
        validators: &[ValidatorVoteKeypairs],
        num_reward_slots: u64,
    ) -> Arc<Bank> {
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
            calc_vote_rewards_update_vote_states(
                &looping_bank,
                Some(reward_cert),
                None,
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos() as i64,
            )
            .unwrap();

            let slot = looping_bank.slot() + 1;
            looping_bank = new_bank_from_parent(looping_bank, slot);
        }
        looping_bank
    }

    /// Progresses the bank a few times to pay out the rewards.
    fn progress_bank_for_payout(mut bank: Arc<Bank>) -> Arc<Bank> {
        for _ in 0..10 {
            let slot = bank.slot() + 1;
            bank = new_bank_from_parent(bank, slot);
        }
        bank
    }

    fn test_vote_reward_payout_impl(
        validators: &[ValidatorVoteKeypairs],
        initial_bank: Arc<Bank>,
        num_reward_slots: u64,
    ) -> Arc<Bank> {
        // bump slots a bit so that reward slots always land in the same epoch and after AG is activated if in migration epoch.
        let slot = initial_bank.slot() + 100_000;
        let initial_bank = new_bank_from_parent(initial_bank, slot);
        let initial_bank_epoch = initial_bank.epoch();
        let rewarded_bank = reward_validators(initial_bank, validators, num_reward_slots);
        assert_eq!(rewarded_bank.epoch(), initial_bank_epoch);

        let payout_epoch = rewarded_bank.epoch() + 1;
        let payout_epoch_slot = rewarded_bank
            .epoch_schedule
            .get_first_slot_in_epoch(payout_epoch);
        let payout_bank = new_bank_from_parent(rewarded_bank, payout_epoch_slot);
        assert_eq!(payout_bank.epoch(), payout_epoch);

        let bank = progress_bank_for_payout(payout_bank);
        assert_eq!(bank.epoch(), initial_bank_epoch + 1);
        bank
    }

    #[test_matrix([true, false], [1_000, 5_000], [0, 10])]
    fn test_vote_reward_payout(pay_leader: bool, commission_bps: u16, num_add_stakers: u64) {
        let num_validators = 2;
        let num_reward_slots = 10;
        let (state, initial_bank) =
            State::new(num_validators, num_add_stakers, pay_leader, commission_bps);
        let final_bank =
            test_vote_reward_payout_impl(&state.validators, initial_bank.clone(), num_reward_slots);
        state.validate_rewards(&initial_bank, &final_bank, num_reward_slots);
    }
}
