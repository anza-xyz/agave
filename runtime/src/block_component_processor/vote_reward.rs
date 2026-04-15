use {
    crate::{bank::Bank, validated_block_finalization::ValidatedBlockFinalizationCert},
    epoch_inflation_account_state::{EpochInflationAccountState, EpochInflationState},
    log::info,
    solana_account::{AccountSharedData, ReadableAccount},
    solana_clock::{Epoch, Slot},
    solana_pubkey::Pubkey,
    solana_vote::vote_account::VoteAccount,
    solana_vote_interface::state::{
        LandedVote, Lockout, MAX_EPOCH_CREDITS_HISTORY, VoteStateV4, VoteStateVersions,
    },
    std::collections::{HashSet, VecDeque},
    thiserror::Error,
};

pub mod epoch_inflation_account_state;

/// Different types of errors that can happen when calculating and paying voting reward.
#[derive(Debug, PartialEq, Eq, Error)]
pub enum PayVoteRewardError {
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
}

/// Calculates voting rewards and updates vote state fields for rewarded validators.
///
/// This is a NOP if [`reward_slot_and_validators`] is [`None`].
///
/// The reward slot is in the past relative to the current slot and hence might be in a different
/// epoch than the current epoch and may have different validator sets and stakes, etc.
/// This function must compute rewards using the stakes in the reward slot and pay them using the
/// stakes in the current slot.
///
/// Additionally we also update vote-state `votes` and `root_slot` fields from the footer
/// reward and finalization certificate
pub(super) fn calculate_and_pay_voting_reward_and_update_vote_state(
    bank: &Bank,
    reward_slot_and_validators: Option<(Slot, Vec<Pubkey>)>,
    final_cert: Option<&ValidatedBlockFinalizationCert>,
) -> Result<(), PayVoteRewardError> {
    let Some((reward_slot, validators_to_reward)) = reward_slot_and_validators else {
        return Ok(());
    };

    debug_assert_eq!(
        validators_to_reward.len(),
        validators_to_reward.iter().collect::<HashSet<_>>().len()
    );

    let current_slot = bank.slot();
    let (reward_slot_accounts, reward_slot_total_stake) = {
        let epoch_stakes = bank.epoch_stakes_from_slot(reward_slot).ok_or(
            PayVoteRewardError::MissingEpochStakes {
                reward_slot,
                current_slot,
            },
        )?;
        (
            epoch_stakes.stakes().vote_accounts().as_ref(),
            epoch_stakes.total_stake(),
        )
    };

    // This assumes that if the epoch_schedule ever changes, the new schedule will maintain correct
    // info about older slots as well.
    let reward_epoch = bank.epoch_schedule.get_epoch(reward_slot);
    let epoch_state = {
        let epoch_inflation_account_state = EpochInflationAccountState::new_from_bank(bank);
        // This function should only be called after alpenglow is active and the slot in the the epoch
        // that activated Alpenglow should have created the account.
        debug_assert!(epoch_inflation_account_state.is_some());
        epoch_inflation_account_state
            .ok_or(PayVoteRewardError::MissingEpochInflationAccountState { current_slot })?
            .get_epoch_state(reward_epoch)
            .ok_or(PayVoteRewardError::NoEpochValidatorStake {
                reward_epoch,
                current_slot,
            })?
    };

    let current_vote_accounts = bank.vote_accounts();
    let current_slot_leader_vote_pubkey = bank.leader().vote_address;
    // Adding 1 to capacity in case the current leader was not in the aggregate and paying it
    // triggers a reallocation.
    let mut paid_vote_accounts = Vec::with_capacity(validators_to_reward.len().saturating_add(1));
    let mut total_leader_reward = 0u64;
    let current_epoch = bank.epoch();
    for validator_to_reward in validators_to_reward {
        let (reward_slot_validator_stake, _) = reward_slot_accounts
            .get(&validator_to_reward)
            .ok_or(PayVoteRewardError::MissingRewardSlotValidator {
                pubkey: validator_to_reward,
                reward_slot,
                current_slot,
            })?;
        let (validator_reward, add_leader_reward) = calculate_reward(
            &epoch_state,
            reward_slot_total_stake,
            *reward_slot_validator_stake,
        );
        total_leader_reward = total_leader_reward.saturating_add(add_leader_reward);

        if current_slot_leader_vote_pubkey == validator_to_reward {
            // Current slot's leader's node pubkey is associated with exactly one vote
            // pubkey and this reward is for this vote pubkey.  The reward will be paid.
            // However, we haven't finished calculating the total rewards for the leader yet.
            // `total_leader_reward` be paid at the end of the function.
            total_leader_reward = total_leader_reward.saturating_add(validator_reward);
            continue;
        }

        // The reward was not for a vote pubkey associated with the current slot's leader's node pubkey.
        // We can pay it into the vote account immediately.
        let Some((_, current_slot_account)) = current_vote_accounts.get(&validator_to_reward)
        else {
            info!(
                "validator {validator_to_reward} was present for reward_slot {reward_slot} but is \
                 absent for current_slot {current_slot}"
            );
            continue;
        };
        if let Some(account_data) = pay_reward_update_vote_state(
            current_epoch,
            reward_slot,
            current_slot_account,
            validator_to_reward,
            validator_reward,
            final_cert,
        ) {
            paid_vote_accounts.push((validator_to_reward, account_data));
        }
    }

    // We have computed the final `total_leader_rewards` now.  We can pay it into the leader's
    // vote account.
    if total_leader_reward != 0 {
        match current_vote_accounts.get(&current_slot_leader_vote_pubkey) {
            Some((_, leader_account)) => {
                if let Some(account_data) = pay_reward_update_vote_state(
                    current_epoch,
                    reward_slot,
                    leader_account,
                    current_slot_leader_vote_pubkey,
                    total_leader_reward,
                    final_cert,
                ) {
                    paid_vote_accounts.push((current_slot_leader_vote_pubkey, account_data));
                }
            }
            None => {
                info!(
                    "Current slot {current_slot}'s leader's account \
                     {current_slot_leader_vote_pubkey} not found.  It will not be paid."
                )
            }
        }
    }

    bank.store_accounts((current_slot, paid_vote_accounts.as_slice()));
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

/// Deserializes `VoteState` from `account`; pays `reward` in `current_epoch` to the `epoch_credits` field;
/// and updates the `votes` and `root_slot` fields in the vote state deserialized from the `account`.
///
/// TODO: this is using VoteStateV4 explicitly.  When we upstream, we will use VoteStateHandle API.
fn pay_reward_update_vote_state(
    current_epoch: Epoch,
    reward_slot: Slot,
    account: &VoteAccount,
    validator_vote_pubkey: Pubkey,
    reward: u64,
    final_cert: Option<&ValidatedBlockFinalizationCert>,
) -> Option<AccountSharedData> {
    let data = account.account().data();
    let Ok(vote_state_versions) = bincode::deserialize(data) else {
        return None;
    };
    match vote_state_versions {
        VoteStateVersions::V4(mut vote_state) => {
            increment_credits(&mut vote_state, current_epoch, reward);
            update_vote_state(
                &mut vote_state,
                reward_slot,
                validator_vote_pubkey,
                final_cert,
            );
            let mut paid_account = AccountSharedData::new(
                account.lamports(),
                account.account().data().len(),
                account.owner(),
            );
            paid_account
                .serialize_data(&VoteStateVersions::V4(vote_state))
                .ok()?;
            Some(paid_account)
        }
        _ => None,
    }
}

/// Stores rewards as credits in the current vote state.
///
/// TODO: this is using VoteStateV4 explicitly.  When we upstream, we will use VoteStateHandle API.
fn increment_credits(vote_state: &mut VoteStateV4, epoch: Epoch, credits: u64) {
    // never seen a credit
    if vote_state.epoch_credits.is_empty() {
        vote_state.epoch_credits.push((epoch, 0, 0));
    } else if epoch != vote_state.epoch_credits.last().unwrap().0 {
        let (_, credits, prev_credits) = *vote_state.epoch_credits.last().unwrap();

        if credits != prev_credits {
            // if credits were earned previous epoch
            // append entry at end of list for the new epoch
            vote_state.epoch_credits.push((epoch, credits, credits));
        } else {
            // else just move the current epoch
            vote_state.epoch_credits.last_mut().unwrap().0 = epoch;
        }

        // Remove too old epoch_credits
        if vote_state.epoch_credits.len() > MAX_EPOCH_CREDITS_HISTORY {
            vote_state.epoch_credits.remove(0);
        }
    }

    vote_state.epoch_credits.last_mut().unwrap().1 = vote_state
        .epoch_credits
        .last()
        .unwrap()
        .1
        .saturating_add(credits);
}

/// Updates `root_slot` and `votes` in vote state using the rewards and finalization certificates from the footer
///
/// Downstream tooling expects these fields to be be set:
/// - `root_slot`, a validator's latest finalized & replayed slot
/// - `votes`, a validator's latest vote
///
/// `votes` is consumed for health monitoring, we require it to be within `DELINQUENT_VALIDATOR_SLOT_DISTANCE`
/// of the tip of the chain to be considered healthy.
///
/// `root_slot` is similarly used by various tooling (e.g. stake delegation) to determine whether the validator
/// is healthy to interact with  and also is required to be within `DELINQUENT_VALIDATOR_SLOT_DISTANCE` of the tip.
///
/// Until we overhaul vote state we continue populating these two fields similar to TowerBFT:
/// - `root_slot` is populated from the footer finalization certificate
/// - `votes` is populated from the footer rewards aggregate
fn update_vote_state(
    vote_state: &mut VoteStateV4,
    reward_slot: Slot,
    validator_vote_pubkey: Pubkey,
    final_cert: Option<&ValidatedBlockFinalizationCert>,
) {
    if let Some(final_cert) = final_cert {
        if final_cert.was_signed_by(&validator_vote_pubkey) {
            vote_state.root_slot = Some(final_cert.slot());
        }
    }

    let latest_root_or_reward = vote_state.root_slot.unwrap_or(reward_slot).max(reward_slot);
    vote_state.votes = VecDeque::from([LandedVote {
        lockout: Lockout::new(latest_root_or_reward),
        latency: 0,
    }]);
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
                create_genesis_config_with_leader_ex,
            },
            inflation_rewards::commission_split,
            stake_utils,
            test_utils::new_rand_vote_account,
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
        solana_bls_signatures::Signature as BLSSignature,
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
        solana_vote_interface::state::VoteStateVersions,
        std::{
            collections::HashMap,
            sync::{Arc, RwLock},
        },
        test_case::test_matrix,
    };

    fn get_vote_state_v4(bank: &Bank, vote_pubkey: &Pubkey) -> VoteStateV4 {
        let vote_accounts = bank.vote_accounts();
        let (_, vote_account) = vote_accounts.get(vote_pubkey).unwrap();
        let vote_state_versions: VoteStateVersions =
            bincode::deserialize(vote_account.account().data()).unwrap();
        let VoteStateVersions::V4(vote_state) = vote_state_versions else {
            panic!("unexpected vote state version");
        };
        *vote_state
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
            signature: BLSSignature::default(),
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
        let mut vote_state = VoteStateV4::default();
        let epoch = 1234;
        let credits = 543432;
        increment_credits(&mut vote_state, epoch, credits);
        assert_eq!(credits, vote_state.epoch_credits.last().unwrap().1);
    }

    #[test]
    fn pay_reward_works() {
        let account =
            VoteAccount::try_from(new_rand_vote_account(&mut rand::rng(), None, true)).unwrap();
        let epoch = 1234;
        let reward = 3453423;
        let account_shared_data =
            pay_reward_update_vote_state(epoch, 0, &account, Pubkey::default(), reward, None)
                .unwrap();
        let vote_state_versions: VoteStateVersions =
            bincode::deserialize(&account_shared_data.data_clone()).unwrap();
        let VoteStateVersions::V4(vote_state) = vote_state_versions else {
            panic!("unexpected state version: {vote_state_versions:?}");
        };
        assert_eq!(reward, vote_state.epoch_credits.last().unwrap().1);
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

        calculate_and_pay_voting_reward_and_update_vote_state(
            &bank,
            Some((reward_slot, validator_pubkeys_to_reward.clone())),
            None,
        )
        .unwrap();

        let vote_accounts = bank.vote_accounts();
        let rewards = validator_pubkeys_to_reward
            .iter()
            .map(|validator| {
                let (_, vote_account) = vote_accounts.get(validator).unwrap();
                let data = vote_account.account().data();
                let vote_state_versions = bincode::deserialize(data).unwrap();
                let VoteStateVersions::V4(vote_state) = vote_state_versions else {
                    panic!();
                };
                assert_eq!(vote_state.epoch_credits.len(), 1);
                let got_reward = vote_state.epoch_credits[0].1;
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

        calculate_and_pay_voting_reward_and_update_vote_state(
            &bank,
            Some((reward_slot, vec![target_vote_pubkey])),
            Some(&final_cert),
        )
        .unwrap();

        let vote_state = get_vote_state_v4(&bank, &target_vote_pubkey);
        assert_eq!(vote_state.root_slot, Some(final_cert.slot()));
        assert_eq!(vote_state.votes.len(), 1);
        assert_eq!(
            vote_state.votes.front().unwrap().lockout.slot(),
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

        calculate_and_pay_voting_reward_and_update_vote_state(
            &bank,
            Some((reward_slot, vec![target_vote_pubkey])),
            Some(&final_cert),
        )
        .unwrap();

        let vote_state = get_vote_state_v4(&bank, &target_vote_pubkey);
        assert_eq!(vote_state.root_slot, None);
        assert_eq!(vote_state.votes.len(), 1);
        assert_eq!(
            vote_state.votes.front().unwrap().lockout.slot(),
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

        calculate_and_pay_voting_reward_and_update_vote_state(
            &bank,
            Some((reward_slot, vec![vote_pubkey])),
            None,
        )
        .unwrap();
        let vote_accounts = bank.vote_accounts();
        for (add, (_, vote_account)) in vote_accounts.iter() {
            let data = vote_account.account().data();
            let vote_state_versions = bincode::deserialize(data).unwrap();
            let VoteStateVersions::V4(vote_state) = vote_state_versions else {
                panic!();
            };
            if add == &vote_pubkey {
                assert!(!vote_state.epoch_credits.is_empty());
            } else {
                assert!(vote_state.epoch_credits.is_empty());
            }
        }
    }

    fn find_leader(validators: &[ValidatorVoteKeypairs]) -> SlotLeader {
        let node_pubkey = validators[0].node_keypair.pubkey();
        let vote_pubkey = validators[0].vote_keypair.pubkey();
        SlotLeader {
            id: node_pubkey,
            vote_address: vote_pubkey,
        }
    }

    fn into_vote_state_v4(account: &Account) -> Box<VoteStateV4> {
        let vote_state_versions = bincode::deserialize(&account.data).unwrap();
        let VoteStateVersions::V4(v4) = vote_state_versions else {
            panic!();
        };
        v4
    }

    fn set_commission(
        genesis_config: &mut GenesisConfig,
        validators: &[ValidatorVoteKeypairs],
        commission_bps: u16,
    ) {
        for validator in validators {
            let vote_pubkey = validator.vote_keypair.pubkey();
            let account = genesis_config.accounts.get_mut(&vote_pubkey).unwrap();
            let mut vote_state = into_vote_state_v4(account);
            vote_state.inflation_rewards_commission_bps = commission_bps;
            VoteStateV4::serialize(
                &VoteStateVersions::V4(vote_state),
                account.data_as_mut_slice(),
            )
            .unwrap();
        }
    }

    #[derive(Debug)]
    struct RewardState {
        vote_pubkey: Pubkey,
        stake_prev_lamports: u64,
        vote_prev_lamports: u64,
        expected_reward: u64,
    }

    #[derive(Debug)]
    struct Staker {
        lamports: u64,
        expected_rewards: u64,
    }

    impl Staker {
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
    struct State {
        voter_lamports: u64,
        voter_expected_reward: u64,
        stakers: HashMap<Pubkey, Staker>,
    }

    impl State {
        fn new(
            bank: &Bank,
            voter_pubkey: Pubkey,
            staker_pubkeys: &[Pubkey],
            rent: &Rent,
            pay_leader: bool,
            commission_bps: u16,
            num_reward_slots: u64,
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

            let vote_rewards = {
                let epoch_state = EpochInflationAccountState::new_from_bank(bank)
                    .unwrap()
                    .get_epoch_state(bank.epoch())
                    .unwrap();
                let total_stake = bank
                    .epoch_stakes_from_slot(bank.slot())
                    .unwrap()
                    .total_stake();
                let (validator_reward, leader_reward) =
                    calculate_reward(&epoch_state, total_stake, validator_stake);
                let vote_rewards = if pay_leader {
                    validator_reward + leader_reward
                } else {
                    validator_reward
                };
                vote_rewards * num_reward_slots
            };

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

            State {
                voter_lamports,
                stakers,
                voter_expected_reward,
            }
        }
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
            let vote_state_versions = bincode::deserialize(vote_account.data()).unwrap();
            let VoteStateVersions::V4(vote_state) = vote_state_versions else {
                panic!();
            };
            assert_eq!(vote_state.epoch_credits.len(), 1);
            let (epoch, final_credit, initial_credit) = vote_state.epoch_credits[0];

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
            calculate_and_pay_voting_reward_and_update_vote_state(
                &looping_bank,
                Some((looping_bank.slot() - 100, validators_to_reward.clone())),
                None,
            )
            .unwrap();

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
        pay_leader: bool,
        commission_bps: u16,
        initial_bank: Option<(Arc<Bank>, Arc<RwLock<BankForks>>)>,
        num_reward_slots: u64,
    ) -> (Arc<Bank>, HashMap<Pubkey, RewardState>) {
        let (initial_bank, _bank_forks) =
            initial_bank.unwrap_or(initial_state(validators, commission_bps));

        let reward_epoch = initial_bank.epoch() + 1;
        let reward_epoch_slot = initial_bank
            .epoch_schedule
            .get_first_slot_in_epoch(reward_epoch);
        let leader = if pay_leader {
            find_leader(validators)
        } else {
            SlotLeader::new_unique()
        };
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
        let payout_bank =
            Bank::new_from_parent(reward_bank, find_leader(validators), payout_epoch_slot);
        assert_eq!(payout_bank.epoch(), payout_epoch);

        // Need to progress banks a few times for the rewards to be paid.
        let mut prev_bank = Arc::new(payout_bank);
        for i in 0..10 {
            let leader = SlotLeader::new_unique();
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
        let (final_bank, rewarded_validators) = test_vote_reward_payout_impl(
            &validators,
            pay_leader,
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
            assert_eq!(vote_cur - state.prev_lamports, state.expected_rewards);
        }
    }

    #[test_matrix([true, false], [1_000, 5_000])]
    fn test_multiple_delegators(pay_leader: bool, commission_bps: u16) {
        let num_reward_slots = 10;
        let mint_keypair = Keypair::new();
        let validator_keypairs = (0..5)
            .map(|_| ValidatorVoteKeypairs::new_rand())
            .collect::<Vec<_>>();
        let validator_lamports = 890_880;
        let mut genesis_config = create_genesis_config_with_leader_ex(
            1_000_000_000,
            &mint_keypair.pubkey(),
            &validator_keypairs[0].node_keypair.pubkey(),
            &validator_keypairs[0].vote_keypair.pubkey(),
            &validator_keypairs[0].stake_keypair.pubkey(),
            Some(
                validator_keypairs[0]
                    .bls_keypair
                    .public
                    .to_bytes_compressed(),
            ),
            LAMPORTS_PER_SOL,
            validator_lamports,
            FeeRateGovernor::new(0, 0),
            Rent::default(),
            ClusterType::Development,
            &FeatureSet::all_enabled(),
            vec![],
        );
        genesis_config.epoch_schedule = EpochSchedule::without_warmup();
        activate_all_features_alpenglow(&mut genesis_config);
        let validators = vec![validator_keypair];
        set_commission(&mut genesis_config, &validators, commission_bps);

        let vote_account = genesis_config
            .accounts
            .get(&validator_vote_key)
            .unwrap()
            .clone()
            .into();

        let staker_keypairs = (0..5).map(|_| Keypair::new()).collect::<Vec<_>>();
        let stake = LAMPORTS_PER_SOL * 2;
        for keypair in &staker_keypairs {
            let stake_pubkey = keypair.pubkey();
            let account = Account::from(stake_utils::create_stake_account(
                &stake_pubkey,
                &validator_vote_key,
                &vote_account,
                &genesis_config.rent,
                stake,
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
            let mut pubkeys = staker_keypairs
                .iter()
                .map(|k| k.pubkey())
                .collect::<Vec<_>>();
            pubkeys.push(validator_stake_key);
            pubkeys
        };
        let prev_state = State::new(
            &initial_bank,
            validator_vote_key,
            &staker_pubkeys,
            &genesis_config.rent,
            pay_leader,
            commission_bps,
            num_reward_slots,
        );

        let (final_bank, _) = test_vote_reward_payout_impl(
            &validators,
            pay_leader,
            commission_bps,
            Some((initial_bank, bank_forks)),
            num_reward_slots,
        );

        let final_state = State::new(
            &final_bank,
            validator_vote_key,
            &staker_pubkeys,
            &genesis_config.rent,
            pay_leader,
            commission_bps,
            num_reward_slots,
        );

        for pubkey in prev_state.stakers.keys() {
            let before = prev_state.stakers.get(pubkey).unwrap();
            let after = final_state.stakers.get(pubkey).unwrap();
            let diff = after.lamports - before.lamports;
            assert_eq!(diff, before.expected_rewards);
        }

        let voter_diff =
            final_state.voter_lamports + VAT_TO_BURN_PER_EPOCH - prev_state.voter_lamports;
        // Due to rounding issues, off by 1 errors are possible.
        assert!(
            voter_diff.abs_diff(prev_state.voter_expected_reward) <= 1,
            "final_lamports={} prev_lamports={} diff={voter_diff} expected={}",
            final_state.voter_lamports,
            prev_state.voter_lamports,
            prev_state.voter_expected_reward
        );
    }
}
