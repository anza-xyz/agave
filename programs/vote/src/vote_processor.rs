//! Vote program processor

use {
    crate::vote_state::{self, handler::VoteStateTargetVersion, NewCommissionCollector},
    log::*,
    solana_bincode::limited_deserialize,
    solana_instruction::error::InstructionError,
    solana_program_runtime::{
        declare_process_instruction, invoke_context::InvokeContext,
        sysvar_cache::get_sysvar_with_account_check,
    },
    solana_pubkey::Pubkey,
    solana_transaction_context::{
        instruction::InstructionContext, instruction_accounts::BorrowedInstructionAccount,
    },
    solana_vote_interface::{instruction::VoteInstruction, program::id, state::VoteAuthorize},
    std::collections::HashSet,
};

#[allow(clippy::too_many_arguments)]
fn process_authorize_with_seed_instruction<F>(
    invoke_context: &InvokeContext,
    instruction_context: &InstructionContext,
    vote_account: &mut BorrowedInstructionAccount,
    target_version: VoteStateTargetVersion,
    new_authority: &Pubkey,
    authorization_type: VoteAuthorize,
    current_authority_derived_key_owner: &Pubkey,
    current_authority_derived_key_seed: &str,
    is_vote_authorize_with_bls_enabled: bool,
    consume_pop_compute_units: F,
) -> Result<(), InstructionError>
where
    F: FnOnce() -> Result<(), InstructionError>,
{
    let clock = get_sysvar_with_account_check::clock(invoke_context, instruction_context, 1)?;
    let mut expected_authority_keys: HashSet<Pubkey> = HashSet::default();
    if instruction_context.is_instruction_account_signer(2)? {
        let base_pubkey = instruction_context.get_key_of_instruction_account(2)?;
        // The conversion from `PubkeyError` to `InstructionError` through
        // num-traits is incorrect, but it's the existing behavior.
        expected_authority_keys.insert(
            Pubkey::create_with_seed(
                base_pubkey,
                current_authority_derived_key_seed,
                current_authority_derived_key_owner,
            )
            .map_err(|e| e as u64)?,
        );
    };
    vote_state::authorize(
        vote_account,
        target_version,
        new_authority,
        authorization_type,
        &expected_authority_keys,
        &clock,
        is_vote_authorize_with_bls_enabled,
        consume_pop_compute_units,
    )
}

fn is_init_account_v2_enabled(invoke_context: &InvokeContext) -> bool {
    let feature_set = invoke_context.get_feature_set();
    feature_set.vote_state_v4
        && feature_set.bls_pubkey_management_in_vote_account
        && feature_set.commission_rate_in_basis_points
        && feature_set.custom_commission_collector
        && feature_set.block_revenue_sharing
        && feature_set.vote_account_initialize_v2
}

fn is_vote_authorize_with_bls_enabled(invoke_context: &InvokeContext) -> bool {
    let feature_set = invoke_context.get_feature_set();
    feature_set.vote_state_v4 && feature_set.bls_pubkey_management_in_vote_account
}

// Citing `runtime/src/block_cost_limit.rs`, vote has statically defined 2100
// units; can consume based on instructions in the future like `bpf_loader` does.
pub const DEFAULT_COMPUTE_UNITS: u64 = 2_100;

/// Cost in compute units for BLS proof-of-possession verification (SIMD-0387).
pub const BLS_PROOF_OF_POSSESSION_VERIFICATION_COMPUTE_UNITS: u64 = 34_500;

declare_process_instruction!(Entrypoint, DEFAULT_COMPUTE_UNITS, |invoke_context| {
    let transaction_context = &invoke_context.transaction_context;
    let instruction_context = transaction_context.get_current_instruction_context()?;
    let data = instruction_context.get_instruction_data();

    trace!("process_instruction: {data:?}");

    let mut me = instruction_context.try_borrow_instruction_account(0)?;
    if *me.get_owner() != id() {
        return Err(InstructionError::InvalidAccountOwner);
    }

    // Determine the target vote state version to use for all operations.
    let target_version = if invoke_context.get_feature_set().vote_state_v4 {
        VoteStateTargetVersion::V4
    } else {
        VoteStateTargetVersion::V3
    };

    let signers = instruction_context.get_signers()?;
    let is_init_account_v2_enabled = is_init_account_v2_enabled(invoke_context);
    let is_vote_authorize_with_bls_enabled = is_vote_authorize_with_bls_enabled(invoke_context);
    let consume_pop_compute_units = || {
        invoke_context
            .consume_checked(BLS_PROOF_OF_POSSESSION_VERIFICATION_COMPUTE_UNITS)
            .map_err(|_| InstructionError::ComputationalBudgetExceeded)
    };
    match limited_deserialize(data, solana_packet::PACKET_DATA_SIZE as u64)? {
        VoteInstruction::InitializeAccount(vote_init) => {
            let rent =
                get_sysvar_with_account_check::rent(invoke_context, &instruction_context, 1)?;
            if !rent.is_exempt(me.get_lamports(), me.get_data().len()) {
                return Err(InstructionError::InsufficientFunds);
            }
            let clock =
                get_sysvar_with_account_check::clock(invoke_context, &instruction_context, 2)?;
            vote_state::initialize_account(&mut me, target_version, &vote_init, &signers, &clock)
        }
        VoteInstruction::Authorize(voter_pubkey, vote_authorize) => {
            let clock =
                get_sysvar_with_account_check::clock(invoke_context, &instruction_context, 1)?;
            vote_state::authorize(
                &mut me,
                target_version,
                &voter_pubkey,
                vote_authorize,
                &signers,
                &clock,
                is_vote_authorize_with_bls_enabled,
                consume_pop_compute_units,
            )
        }
        VoteInstruction::AuthorizeWithSeed(args) => {
            instruction_context.check_number_of_instruction_accounts(3)?;
            process_authorize_with_seed_instruction(
                invoke_context,
                &instruction_context,
                &mut me,
                target_version,
                &args.new_authority,
                args.authorization_type,
                &args.current_authority_derived_key_owner,
                args.current_authority_derived_key_seed.as_str(),
                is_vote_authorize_with_bls_enabled,
                consume_pop_compute_units,
            )
        }
        VoteInstruction::AuthorizeCheckedWithSeed(args) => {
            instruction_context.check_number_of_instruction_accounts(4)?;
            let new_authority = instruction_context.get_key_of_instruction_account(3)?;
            if !instruction_context.is_instruction_account_signer(3)? {
                return Err(InstructionError::MissingRequiredSignature);
            }
            process_authorize_with_seed_instruction(
                invoke_context,
                &instruction_context,
                &mut me,
                target_version,
                new_authority,
                args.authorization_type,
                &args.current_authority_derived_key_owner,
                args.current_authority_derived_key_seed.as_str(),
                is_vote_authorize_with_bls_enabled,
                consume_pop_compute_units,
            )
        }
        VoteInstruction::UpdateValidatorIdentity => {
            instruction_context.check_number_of_instruction_accounts(2)?;
            let node_pubkey = instruction_context.get_key_of_instruction_account(1)?;
            let custom_collector_enabled =
                invoke_context.get_feature_set().custom_commission_collector;
            vote_state::update_validator_identity(
                &mut me,
                target_version,
                node_pubkey,
                &signers,
                custom_collector_enabled,
            )
        }
        VoteInstruction::UpdateCommission(commission) => {
            let sysvar_cache = invoke_context.get_sysvar_cache();

            // Disable the commission update rule after the "delay commission
            // update" feature is activated because it imposes a minimum delay
            // of one full epoch before the new commission rate takes effect.
            let disable_commission_update_rule =
                invoke_context.get_feature_set().delay_commission_updates;

            vote_state::update_commission(
                &mut me,
                target_version,
                commission,
                &signers,
                sysvar_cache.get_epoch_schedule()?.as_ref(),
                sysvar_cache.get_clock()?.as_ref(),
                disable_commission_update_rule,
            )
        }
        VoteInstruction::Vote(vote) | VoteInstruction::VoteSwitch(vote, _) => {
            if invoke_context.is_deprecate_legacy_vote_ixs_active() {
                return Err(InstructionError::InvalidInstructionData);
            }
            let slot_hashes = get_sysvar_with_account_check::slot_hashes(
                invoke_context,
                &instruction_context,
                1,
            )?;
            let clock =
                get_sysvar_with_account_check::clock(invoke_context, &instruction_context, 2)?;
            vote_state::process_vote_with_account(
                &mut me,
                target_version,
                &slot_hashes,
                &clock,
                &vote,
                &signers,
            )
        }
        VoteInstruction::UpdateVoteState(vote_state_update)
        | VoteInstruction::UpdateVoteStateSwitch(vote_state_update, _) => {
            if invoke_context.is_deprecate_legacy_vote_ixs_active() {
                return Err(InstructionError::InvalidInstructionData);
            }
            let sysvar_cache = invoke_context.get_sysvar_cache();
            let slot_hashes = sysvar_cache.get_slot_hashes()?;
            let clock = sysvar_cache.get_clock()?;
            vote_state::process_vote_state_update(
                &mut me,
                target_version,
                slot_hashes.slot_hashes(),
                &clock,
                vote_state_update,
                &signers,
            )
        }
        VoteInstruction::CompactUpdateVoteState(vote_state_update)
        | VoteInstruction::CompactUpdateVoteStateSwitch(vote_state_update, _) => {
            if invoke_context.is_deprecate_legacy_vote_ixs_active() {
                return Err(InstructionError::InvalidInstructionData);
            }
            let sysvar_cache = invoke_context.get_sysvar_cache();
            let slot_hashes = sysvar_cache.get_slot_hashes()?;
            let clock = sysvar_cache.get_clock()?;
            vote_state::process_vote_state_update(
                &mut me,
                target_version,
                slot_hashes.slot_hashes(),
                &clock,
                vote_state_update,
                &signers,
            )
        }
        VoteInstruction::TowerSync(tower_sync)
        | VoteInstruction::TowerSyncSwitch(tower_sync, _) => {
            let sysvar_cache = invoke_context.get_sysvar_cache();
            let slot_hashes = sysvar_cache.get_slot_hashes()?;
            let clock = sysvar_cache.get_clock()?;
            vote_state::process_tower_sync(
                &mut me,
                target_version,
                slot_hashes.slot_hashes(),
                &clock,
                tower_sync,
                &signers,
            )
        }
        VoteInstruction::Withdraw(lamports) => {
            instruction_context.check_number_of_instruction_accounts(2)?;
            let rent_sysvar = invoke_context.get_sysvar_cache().get_rent()?;
            let clock_sysvar = invoke_context.get_sysvar_cache().get_clock()?;

            drop(me);
            vote_state::withdraw(
                &instruction_context,
                0,
                target_version,
                lamports,
                1,
                &signers,
                &rent_sysvar,
                &clock_sysvar,
            )
        }
        VoteInstruction::AuthorizeChecked(vote_authorize) => {
            instruction_context.check_number_of_instruction_accounts(4)?;
            let voter_pubkey = instruction_context.get_key_of_instruction_account(3)?;
            if !instruction_context.is_instruction_account_signer(3)? {
                return Err(InstructionError::MissingRequiredSignature);
            }
            let clock =
                get_sysvar_with_account_check::clock(invoke_context, &instruction_context, 1)?;
            vote_state::authorize(
                &mut me,
                target_version,
                voter_pubkey,
                vote_authorize,
                &signers,
                &clock,
                is_vote_authorize_with_bls_enabled,
                consume_pop_compute_units,
            )
        }
        VoteInstruction::InitializeAccountV2(vote_init_v2) => {
            if !is_init_account_v2_enabled {
                return Err(InstructionError::InvalidInstructionData);
            }
            let rent = invoke_context.get_sysvar_cache().get_rent()?;
            if !rent.is_exempt(me.get_lamports(), me.get_data().len()) {
                return Err(InstructionError::InsufficientFunds);
            }
            let clock = invoke_context.get_sysvar_cache().get_clock()?;
            vote_state::initialize_account_v2(
                &mut me,
                target_version,
                &vote_init_v2,
                &signers,
                &clock,
                consume_pop_compute_units,
            )
        }
        VoteInstruction::UpdateCommissionBps {
            commission_bps,
            kind,
        } => {
            // SIMD-0291: Commission Rate in Basis Points
            // Requires SIMD-0185: Vote State V4
            // Requires SIMD-0249: Delay Commission Updates
            let feature_set = invoke_context.get_feature_set();
            if !feature_set.commission_rate_in_basis_points
                || !feature_set.delay_commission_updates
                || !matches!(target_version, VoteStateTargetVersion::V4)
            {
                return Err(InstructionError::InvalidInstructionData);
            }
            vote_state::update_commission_bps(
                &mut me,
                target_version,
                commission_bps,
                kind,
                &signers,
                feature_set.block_revenue_sharing,
            )
        }
        VoteInstruction::UpdateCommissionCollector(kind) => {
            // SIMD-0232: Custom Commission Collector Account
            // Requires SIMD-0185: Vote State V4
            let custom_collector_enabled =
                invoke_context.get_feature_set().custom_commission_collector;
            if !(custom_collector_enabled && matches!(target_version, VoteStateTargetVersion::V4)) {
                return Err(InstructionError::InvalidInstructionData);
            }

            let new_collector = if instruction_context.get_key_of_instruction_account(1)?
                == me.get_key()
            {
                NewCommissionCollector::VoteAccount
            } else {
                let collector_account = instruction_context.try_borrow_instruction_account(1)?;
                NewCommissionCollector::NewAccount(collector_account)
            };

            let rent = invoke_context.get_sysvar_cache().get_rent()?;

            vote_state::update_commission_collector(
                &mut me,
                target_version,
                new_collector,
                kind,
                &signers,
                &rent,
            )
        }
        VoteInstruction::DepositDelegatorRewards { deposit } => {
            // SIMD-0123: Deposit delegator rewards.
            // Requires:
            // * SIMD-0185: Vote State V4
            // * SIMD-0291: Commission in Basis Points
            // * SIMD-0232: Custom Commission Collector
            let feature_set = invoke_context.get_feature_set();
            if !feature_set.commission_rate_in_basis_points
                || !feature_set.custom_commission_collector
                || !feature_set.block_revenue_sharing
                || !matches!(target_version, VoteStateTargetVersion::V4)
            {
                return Err(InstructionError::InvalidInstructionData);
            }

            instruction_context.check_number_of_instruction_accounts(2)?;
            drop(me);
            vote_state::deposit_delegator_rewards(invoke_context, deposit)
        }
    }
});

#[allow(clippy::arithmetic_side_effects)]
#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            vote_error::VoteError,
            vote_instruction::{
                authorize, authorize_checked, compact_update_vote_state,
                compact_update_vote_state_switch, create_account_with_config, update_commission,
                update_validator_identity, update_vote_state, update_vote_state_switch, vote,
                vote_switch, withdraw, CreateVoteAccountConfig, VoteInstruction,
            },
            vote_state::{
                self, create_bls_pubkey_and_proof_of_possession,
                handler::{VoteStateHandle, VoteStateHandler},
                Lockout, TowerSync, Vote, VoteAuthorize, VoteAuthorizeCheckedWithSeedArgs,
                VoteAuthorizeWithSeedArgs, VoteInit, VoteInitV2, VoteStateUpdate, VoteStateV3,
                VoteStateV4, VoteStateVersions,
            },
        },
        bincode::serialize,
        solana_account::{
            self as account, state_traits::StateMut, Account, AccountSharedData, ReadableAccount,
            WritableAccount,
        },
        solana_clock::Clock,
        solana_epoch_schedule::EpochSchedule,
        solana_hash::Hash,
        solana_instruction::{AccountMeta, Instruction},
        solana_program_runtime::{
            invoke_context::mock_process_instruction_with_feature_set,
            loaded_programs::ProgramCacheEntry, solana_sbpf::vm::ContextObject,
        },
        solana_pubkey::Pubkey,
        solana_rent::Rent,
        solana_sdk_ids::sysvar,
        solana_slot_hashes::SlotHashes,
        solana_svm_feature_set::SVMFeatureSet,
        solana_system_program::system_processor::DEFAULT_COMPUTE_UNITS as SYSTEM_PROGRAM_COMPUTE_UNITS,
        solana_vote_interface::{
            instruction::{tower_sync, tower_sync_switch, CommissionKind},
            state::{
                VoterWithBLSArgs, BLS_PROOF_OF_POSSESSION_COMPRESSED_SIZE,
                BLS_PUBLIC_KEY_COMPRESSED_SIZE,
            },
        },
        std::{cell::RefCell, collections::HashSet, str::FromStr, sync::Arc},
        test_case::{test_case, test_matrix},
    };

    // They're the same, but just for posterity.
    fn vote_state_size_of(vote_state_v4_enabled: bool) -> usize {
        if vote_state_v4_enabled {
            VoteStateV4::size_of()
        } else {
            VoteStateV3::size_of()
        }
    }

    fn deserialize_vote_state_for_test(
        vote_state_v4_enabled: bool,
        account_data: &[u8],
        vote_pubkey: &Pubkey,
    ) -> VoteStateHandler {
        if vote_state_v4_enabled {
            VoteStateHandler::new_v4(VoteStateV4::deserialize(account_data, vote_pubkey).unwrap())
        } else {
            VoteStateHandler::new_v3(VoteStateV3::deserialize(account_data).unwrap())
        }
    }

    struct VoteAccountTestFixtureWithAuthorities {
        vote_account: AccountSharedData,
        vote_pubkey: Pubkey,
        voter_base_key: Pubkey,
        voter_owner: Pubkey,
        voter_seed: String,
        withdrawer_base_key: Pubkey,
        withdrawer_owner: Pubkey,
        withdrawer_seed: String,
    }

    fn create_default_account() -> AccountSharedData {
        AccountSharedData::new(0, 0, &Pubkey::new_unique())
    }

    #[derive(Clone, Copy, Default)]
    struct VoteProgramFeatures {
        vote_state_v4: bool,
        bls_pubkey_management_in_vote_account: bool,
        commission_rate_in_basis_points: bool,
        custom_commission_collector: bool,
        block_revenue_sharing: bool,
        vote_account_initialize_v2: bool,
    }

    impl VoteProgramFeatures {
        fn all_enabled() -> Self {
            Self {
                vote_state_v4: true,
                bls_pubkey_management_in_vote_account: true,
                commission_rate_in_basis_points: true,
                custom_commission_collector: true,
                block_revenue_sharing: true,
                vote_account_initialize_v2: true,
            }
        }
    }

    fn process_instruction(
        features: VoteProgramFeatures,
        instruction_data: &[u8],
        transaction_accounts: Vec<(Pubkey, AccountSharedData)>,
        instruction_accounts: Vec<AccountMeta>,
        expected_result: Result<(), InstructionError>,
    ) -> Vec<AccountSharedData> {
        process_instruction_with_cu_check(
            features,
            instruction_data,
            transaction_accounts,
            instruction_accounts,
            expected_result,
            DEFAULT_COMPUTE_UNITS,
        )
    }

    fn process_instruction_with_cu_check(
        features: VoteProgramFeatures,
        instruction_data: &[u8],
        transaction_accounts: Vec<(Pubkey, AccountSharedData)>,
        instruction_accounts: Vec<AccountMeta>,
        expected_result: Result<(), InstructionError>,
        expected_cus: u64,
    ) -> Vec<AccountSharedData> {
        let VoteProgramFeatures {
            vote_state_v4,
            bls_pubkey_management_in_vote_account,
            commission_rate_in_basis_points,
            custom_commission_collector,
            block_revenue_sharing,
            vote_account_initialize_v2,
        } = features;
        let cu_consumed = RefCell::new(0u64);
        let accounts = mock_process_instruction_with_feature_set(
            &id(),
            None,
            instruction_data,
            transaction_accounts,
            instruction_accounts,
            expected_result,
            Entrypoint::vm,
            |invoke_context| {
                // Register system program for CPI support.
                invoke_context.program_cache_for_tx_batch.replenish(
                    solana_sdk_ids::system_program::id(),
                    Arc::new(ProgramCacheEntry::new_builtin(
                        0,
                        0,
                        solana_system_program::system_processor::Entrypoint::vm,
                    )),
                );
                *cu_consumed.borrow_mut() = invoke_context.get_remaining();
            },
            |invoke_context| {
                *cu_consumed.borrow_mut() -= invoke_context.get_remaining();
            },
            &SVMFeatureSet {
                vote_state_v4,
                bls_pubkey_management_in_vote_account,
                commission_rate_in_basis_points,
                custom_commission_collector,
                block_revenue_sharing,
                vote_account_initialize_v2,
                ..SVMFeatureSet::all_enabled()
            },
        );
        assert_eq!(
            *cu_consumed.borrow(),
            expected_cus,
            "Expected {} CU consumed, got {}",
            expected_cus,
            *cu_consumed.borrow()
        );
        accounts
    }

    fn process_instruction_as_one_arg(
        features: VoteProgramFeatures,
        instruction: &Instruction,
        expected_result: Result<(), InstructionError>,
    ) -> Vec<AccountSharedData> {
        process_instruction_as_one_arg_with_cu_check(
            features,
            instruction,
            expected_result,
            DEFAULT_COMPUTE_UNITS,
        )
    }

    fn process_instruction_as_one_arg_with_cu_check(
        features: VoteProgramFeatures,
        instruction: &Instruction,
        expected_result: Result<(), InstructionError>,
        expected_cus: u64,
    ) -> Vec<AccountSharedData> {
        let mut pubkeys: HashSet<Pubkey> = instruction
            .accounts
            .iter()
            .map(|meta| meta.pubkey)
            .collect();
        pubkeys.insert(sysvar::clock::id());
        pubkeys.insert(sysvar::epoch_schedule::id());
        pubkeys.insert(sysvar::rent::id());
        pubkeys.insert(sysvar::slot_hashes::id());
        let transaction_accounts: Vec<_> = pubkeys
            .iter()
            .map(|pubkey| {
                (
                    *pubkey,
                    if sysvar::clock::check_id(pubkey) {
                        account::create_account_shared_data_for_test(&Clock::default())
                    } else if sysvar::epoch_schedule::check_id(pubkey) {
                        account::create_account_shared_data_for_test(
                            &EpochSchedule::without_warmup(),
                        )
                    } else if sysvar::slot_hashes::check_id(pubkey) {
                        account::create_account_shared_data_for_test(&SlotHashes::default())
                    } else if sysvar::rent::check_id(pubkey) {
                        account::create_account_shared_data_for_test(&Rent::free())
                    } else if *pubkey == invalid_vote_state_pubkey() {
                        AccountSharedData::from(Account {
                            owner: invalid_vote_state_pubkey(),
                            ..Account::default()
                        })
                    } else {
                        AccountSharedData::from(Account {
                            owner: id(),
                            ..Account::default()
                        })
                    },
                )
            })
            .collect();
        process_instruction_with_cu_check(
            features,
            &instruction.data,
            transaction_accounts,
            instruction.accounts.clone(),
            expected_result,
            expected_cus,
        )
    }

    fn invalid_vote_state_pubkey() -> Pubkey {
        Pubkey::from_str("BadVote111111111111111111111111111111111111").unwrap()
    }

    fn create_default_rent_account() -> AccountSharedData {
        account::create_account_shared_data_for_test(&Rent::free())
    }

    fn create_default_clock_account() -> AccountSharedData {
        account::create_account_shared_data_for_test(&Clock::default())
    }

    fn create_test_account(vote_state_v4_enabled: bool) -> (Pubkey, AccountSharedData) {
        let rent = Rent::default();
        let vote_pubkey = solana_pubkey::new_rand();
        let node_pubkey = solana_pubkey::new_rand();

        let account = if vote_state_v4_enabled {
            let balance = rent.minimum_balance(VoteStateV4::size_of());
            vote_state::create_v4_account_with_authorized(
                &node_pubkey,
                &vote_pubkey,
                [0u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE],
                &vote_pubkey,
                0,
                &vote_pubkey,
                0,
                &vote_pubkey,
                balance,
            )
        } else {
            let balance = rent.minimum_balance(vote_state_size_of(vote_state_v4_enabled));
            vote_state::create_v3_account_with_authorized(
                &node_pubkey,
                &vote_pubkey,
                &vote_pubkey,
                0,
                balance,
            )
        };

        (vote_pubkey, account)
    }

    fn create_test_account_with_authorized(
        vote_state_v4_enabled: bool,
    ) -> (Pubkey, Pubkey, Pubkey, AccountSharedData) {
        let vote_pubkey = solana_pubkey::new_rand();
        let authorized_voter = solana_pubkey::new_rand();
        let authorized_withdrawer = solana_pubkey::new_rand();
        let account = create_test_account_with_provided_authorized(
            &authorized_voter,
            &authorized_withdrawer,
            vote_state_v4_enabled,
        );

        (
            vote_pubkey,
            authorized_voter,
            authorized_withdrawer,
            account,
        )
    }

    fn create_test_account_with_provided_authorized(
        authorized_voter: &Pubkey,
        authorized_withdrawer: &Pubkey,
        vote_state_v4_enabled: bool,
    ) -> AccountSharedData {
        let node_pubkey = solana_pubkey::new_rand();

        if vote_state_v4_enabled {
            vote_state::create_v4_account_with_authorized(
                &node_pubkey,
                authorized_voter,
                [0u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE],
                authorized_withdrawer,
                0,
                authorized_withdrawer,
                0,
                authorized_withdrawer,
                100,
            )
        } else {
            vote_state::create_v3_account_with_authorized(
                &node_pubkey,
                authorized_voter,
                authorized_withdrawer,
                0,
                100,
            )
        }
    }

    fn create_test_account_with_authorized_from_seed(
        vote_state_v4_enabled: bool,
    ) -> VoteAccountTestFixtureWithAuthorities {
        let vote_pubkey = Pubkey::new_unique();
        let voter_base_key = Pubkey::new_unique();
        let voter_owner = Pubkey::new_unique();
        let voter_seed = String::from("VOTER_SEED");
        let withdrawer_base_key = Pubkey::new_unique();
        let withdrawer_owner = Pubkey::new_unique();
        let withdrawer_seed = String::from("WITHDRAWER_SEED");
        let authorized_voter =
            Pubkey::create_with_seed(&voter_base_key, voter_seed.as_str(), &voter_owner).unwrap();
        let authorized_withdrawer = Pubkey::create_with_seed(
            &withdrawer_base_key,
            withdrawer_seed.as_str(),
            &withdrawer_owner,
        )
        .unwrap();

        let node_pubkey = Pubkey::new_unique();
        let vote_account = if vote_state_v4_enabled {
            vote_state::create_v4_account_with_authorized(
                &node_pubkey,
                &authorized_voter,
                [0u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE],
                &authorized_withdrawer,
                0,
                &authorized_withdrawer,
                0,
                &authorized_withdrawer,
                100,
            )
        } else {
            vote_state::create_v3_account_with_authorized(
                &node_pubkey,
                &authorized_voter,
                &authorized_withdrawer,
                0,
                100,
            )
        };

        VoteAccountTestFixtureWithAuthorities {
            vote_account,
            vote_pubkey,
            voter_base_key,
            voter_owner,
            voter_seed,
            withdrawer_base_key,
            withdrawer_owner,
            withdrawer_seed,
        }
    }

    fn create_test_account_with_epoch_credits(
        vote_state_v4_enabled: bool,
        credits_to_append: &[u64],
    ) -> (Pubkey, AccountSharedData) {
        let vote_pubkey = solana_pubkey::new_rand();
        let node_pubkey = solana_pubkey::new_rand();

        let vote_init = VoteInit {
            node_pubkey,
            authorized_voter: vote_pubkey,
            authorized_withdrawer: vote_pubkey,
            commission: 0,
        };
        let clock = Clock::default();

        let space = vote_state_size_of(vote_state_v4_enabled);
        let lamports = Rent::default().minimum_balance(space);

        let mut vote_state = if vote_state_v4_enabled {
            let v4 = VoteStateV4::new_with_defaults(&vote_pubkey, &vote_init, &clock);
            VoteStateHandler::new_v4(v4)
        } else {
            let v3 = VoteStateV3::new(&vote_init, &clock);
            VoteStateHandler::new_v3(v3)
        };

        let epoch_credits = vote_state.epoch_credits_mut();
        epoch_credits.clear();

        let mut current_epoch_credits: u64 = 0;
        let mut previous_epoch_credits = 0;
        for (epoch, credits) in credits_to_append.iter().enumerate() {
            current_epoch_credits = current_epoch_credits.saturating_add(*credits);
            epoch_credits.push((
                u64::try_from(epoch).unwrap(),
                current_epoch_credits,
                previous_epoch_credits,
            ));
            previous_epoch_credits = current_epoch_credits;
        }

        let mut account = AccountSharedData::new(lamports, space, &id());
        account.set_data_from_slice(&vote_state.serialize());

        (vote_pubkey, account)
    }

    /// Returns Vec of serialized VoteInstruction and flag indicating if it is a tower sync
    /// variant, along with the original vote
    fn create_serialized_votes() -> (Vote, Vec<(Vec<u8>, bool)>) {
        let vote = Vote::new(vec![1], Hash::default());
        let vote_state_update = VoteStateUpdate::from(vec![(1, 1)]);
        let tower_sync = TowerSync::from(vec![(1, 1)]);
        (
            vote.clone(),
            vec![
                (serialize(&VoteInstruction::Vote(vote)).unwrap(), false),
                (
                    serialize(&VoteInstruction::UpdateVoteState(vote_state_update.clone()))
                        .unwrap(),
                    false,
                ),
                (
                    serialize(&VoteInstruction::CompactUpdateVoteState(vote_state_update)).unwrap(),
                    false,
                ),
                (
                    serialize(&VoteInstruction::TowerSync(tower_sync)).unwrap(),
                    true,
                ),
            ],
        )
    }

    #[test_case(false ; "VoteStateV3")]
    #[test_case(true ; "VoteStateV4")]
    fn test_vote_process_instruction_decode_bail(vote_state_v4: bool) {
        process_instruction(
            VoteProgramFeatures {
                vote_state_v4,
                ..Default::default()
            },
            &[],
            Vec::new(),
            Vec::new(),
            Err(InstructionError::MissingAccount),
        );
    }

    #[test_matrix(
        [false, true],
        [false, true],
        [false, true],
        [false, true],
        [false, true],
        [false, true]
    )]
    fn test_initialize_vote_account(
        vote_state_v4: bool,
        bls_pubkey_management_in_vote_account: bool,
        commission_rate_in_basis_points: bool,
        custom_commission_collector: bool,
        block_revenue_sharing: bool,
        vote_account_initialize_v2: bool,
    ) {
        let vote_pubkey = solana_pubkey::new_rand();
        let vote_account = AccountSharedData::new(100, vote_state_size_of(vote_state_v4), &id());
        let node_pubkey = solana_pubkey::new_rand();
        let node_account = AccountSharedData::default();
        let instruction_data = serialize(&VoteInstruction::InitializeAccount(VoteInit {
            node_pubkey,
            authorized_voter: vote_pubkey,
            authorized_withdrawer: vote_pubkey,
            commission: 0,
        }))
        .unwrap();
        let mut instruction_accounts = vec![
            AccountMeta {
                pubkey: vote_pubkey,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: sysvar::rent::id(),
                is_signer: false,
                is_writable: false,
            },
            AccountMeta {
                pubkey: sysvar::clock::id(),
                is_signer: false,
                is_writable: false,
            },
            AccountMeta {
                pubkey: node_pubkey,
                is_signer: true,
                is_writable: false,
            },
        ];

        let features = VoteProgramFeatures {
            vote_state_v4,
            bls_pubkey_management_in_vote_account,
            commission_rate_in_basis_points,
            custom_commission_collector,
            block_revenue_sharing,
            vote_account_initialize_v2,
        };

        let accounts = process_instruction(
            features,
            &instruction_data,
            vec![
                (vote_pubkey, vote_account.clone()),
                (sysvar::rent::id(), create_default_rent_account()),
                (sysvar::clock::id(), create_default_clock_account()),
                (node_pubkey, node_account.clone()),
            ],
            instruction_accounts.clone(),
            Ok(()),
        );

        // reinit should fail
        process_instruction(
            features,
            &instruction_data,
            vec![
                (vote_pubkey, accounts[0].clone()),
                (sysvar::rent::id(), create_default_rent_account()),
                (sysvar::clock::id(), create_default_clock_account()),
                (node_pubkey, accounts[3].clone()),
            ],
            instruction_accounts.clone(),
            Err(InstructionError::AccountAlreadyInitialized),
        );

        // init should fail, account is too big
        process_instruction(
            features,
            &instruction_data,
            vec![
                (
                    vote_pubkey,
                    AccountSharedData::new(100, 2 * vote_state_size_of(vote_state_v4), &id()),
                ),
                (sysvar::rent::id(), create_default_rent_account()),
                (sysvar::clock::id(), create_default_clock_account()),
                (node_pubkey, node_account.clone()),
            ],
            instruction_accounts.clone(),
            Err(InstructionError::InvalidAccountData),
        );

        // init should fail, node_pubkey didn't sign the transaction
        instruction_accounts[3].is_signer = false;
        process_instruction(
            features,
            &instruction_data,
            vec![
                (vote_pubkey, vote_account.clone()),
                (sysvar::rent::id(), create_default_rent_account()),
                (sysvar::clock::id(), create_default_clock_account()),
                (node_pubkey, node_account.clone()),
            ],
            instruction_accounts.clone(),
            Err(InstructionError::MissingRequiredSignature),
        );
    }

    #[test_matrix(
        [false, true],
        [false, true],
        [false, true],
        [false, true],
        [false, true],
        [false, true]
    )]
    fn test_initialize_vote_account_v2(
        vote_state_v4: bool,
        bls_pubkey_management_in_vote_account: bool,
        commission_rate_in_basis_points: bool,
        custom_commission_collector: bool,
        block_revenue_sharing: bool,
        vote_account_initialize_v2: bool,
    ) {
        let vote_pubkey = solana_pubkey::new_rand();
        let vote_account = AccountSharedData::new(100, vote_state_size_of(vote_state_v4), &id());
        let node_pubkey = solana_pubkey::new_rand();
        let node_account = AccountSharedData::default();
        let (bls_pubkey, bls_proof_of_possession) =
            create_bls_pubkey_and_proof_of_possession(&vote_pubkey);
        let instruction_data = serialize(&VoteInstruction::InitializeAccountV2(VoteInitV2 {
            node_pubkey,
            authorized_voter: vote_pubkey,
            authorized_withdrawer: vote_pubkey,
            authorized_voter_bls_pubkey: bls_pubkey,
            authorized_voter_bls_proof_of_possession: bls_proof_of_possession,
            ..Default::default()
        }))
        .unwrap();
        let mut instruction_accounts = vec![
            AccountMeta {
                pubkey: vote_pubkey,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: sysvar::rent::id(),
                is_signer: false,
                is_writable: false,
            },
            AccountMeta {
                pubkey: sysvar::clock::id(),
                is_signer: false,
                is_writable: false,
            },
            AccountMeta {
                pubkey: node_pubkey,
                is_signer: true,
                is_writable: false,
            },
        ];

        let features = VoteProgramFeatures {
            vote_state_v4,
            bls_pubkey_management_in_vote_account,
            commission_rate_in_basis_points,
            custom_commission_collector,
            block_revenue_sharing,
            vote_account_initialize_v2,
        };

        let all_v2_features_enabled = vote_state_v4
            && bls_pubkey_management_in_vote_account
            && commission_rate_in_basis_points
            && custom_commission_collector
            && block_revenue_sharing
            && vote_account_initialize_v2;

        // If any v2 feature is disabled, the new instruction should be rejected
        if !all_v2_features_enabled {
            process_instruction(
                features,
                &instruction_data,
                vec![
                    (vote_pubkey, vote_account.clone()),
                    (sysvar::rent::id(), create_default_rent_account()),
                    (sysvar::clock::id(), create_default_clock_account()),
                    (node_pubkey, node_account.clone()),
                ],
                instruction_accounts.clone(),
                Err(InstructionError::InvalidInstructionData),
            );
            return;
        }

        let accounts = process_instruction_with_cu_check(
            features,
            &instruction_data,
            vec![
                (vote_pubkey, vote_account.clone()),
                (sysvar::rent::id(), create_default_rent_account()),
                (sysvar::clock::id(), create_default_clock_account()),
                (node_pubkey, node_account.clone()),
            ],
            instruction_accounts.clone(),
            Ok(()),
            DEFAULT_COMPUTE_UNITS + BLS_PROOF_OF_POSSESSION_VERIFICATION_COMPUTE_UNITS,
        );

        // reinit should fail
        process_instruction(
            features,
            &instruction_data,
            vec![
                (vote_pubkey, accounts[0].clone()),
                (sysvar::rent::id(), create_default_rent_account()),
                (sysvar::clock::id(), create_default_clock_account()),
                (node_pubkey, accounts[3].clone()),
            ],
            instruction_accounts.clone(),
            Err(InstructionError::AccountAlreadyInitialized),
        );

        // init should fail, account is too big
        process_instruction(
            features,
            &instruction_data,
            vec![
                (
                    vote_pubkey,
                    AccountSharedData::new(100, 2 * vote_state_size_of(vote_state_v4), &id()),
                ),
                (sysvar::rent::id(), create_default_rent_account()),
                (sysvar::clock::id(), create_default_clock_account()),
                (node_pubkey, node_account.clone()),
            ],
            instruction_accounts.clone(),
            Err(InstructionError::InvalidAccountData),
        );

        // init should fail, node_pubkey didn't sign the transaction
        instruction_accounts[3].is_signer = false;
        process_instruction(
            features,
            &instruction_data,
            vec![
                (vote_pubkey, vote_account.clone()),
                (sysvar::rent::id(), create_default_rent_account()),
                (sysvar::clock::id(), create_default_clock_account()),
                (node_pubkey, node_account.clone()),
            ],
            instruction_accounts.clone(),
            Err(InstructionError::MissingRequiredSignature),
        );
    }

    #[test]
    fn test_initialize_vote_account_v2_bad_proof_of_possession() {
        let vote_pubkey = solana_pubkey::new_rand();
        let vote_account = AccountSharedData::new(100, VoteStateV4::size_of(), &id());
        let node_pubkey = solana_pubkey::new_rand();
        let node_account = AccountSharedData::default();
        let instruction_with_bad_pop =
            serialize(&VoteInstruction::InitializeAccountV2(VoteInitV2 {
                node_pubkey,
                authorized_voter: vote_pubkey,
                authorized_withdrawer: vote_pubkey,
                authorized_voter_bls_pubkey: [1u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE],
                authorized_voter_bls_proof_of_possession: [2u8;
                    BLS_PROOF_OF_POSSESSION_COMPRESSED_SIZE],
                ..Default::default()
            }))
            .unwrap();
        let instruction_accounts = vec![
            AccountMeta {
                pubkey: vote_pubkey,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: sysvar::rent::id(),
                is_signer: false,
                is_writable: false,
            },
            AccountMeta {
                pubkey: sysvar::clock::id(),
                is_signer: false,
                is_writable: false,
            },
            AccountMeta {
                pubkey: node_pubkey,
                is_signer: true,
                is_writable: false,
            },
        ];
        process_instruction_with_cu_check(
            VoteProgramFeatures::all_enabled(),
            &instruction_with_bad_pop,
            vec![
                (vote_pubkey, vote_account),
                (sysvar::rent::id(), create_default_rent_account()),
                (sysvar::clock::id(), create_default_clock_account()),
                (node_pubkey, node_account),
            ],
            instruction_accounts,
            Err(InstructionError::InvalidArgument),
            DEFAULT_COMPUTE_UNITS + BLS_PROOF_OF_POSSESSION_VERIFICATION_COMPUTE_UNITS,
        );
    }

    #[test_matrix([false, true], [false, true])]
    fn test_vote_update_validator_identity(vote_state_v4: bool, custom_commission_collector: bool) {
        let (vote_pubkey, _authorized_voter, authorized_withdrawer, vote_account) =
            create_test_account_with_authorized(vote_state_v4);

        let original_block_revenue_collector = {
            // We only need this check for `vote_state_v4=true`.
            if vote_state_v4 {
                let vote_state = deserialize_vote_state_for_test(
                    vote_state_v4,
                    vote_account.data(),
                    &vote_pubkey,
                );
                let block_revenue_collector = vote_state.as_ref_v4().block_revenue_collector;
                Some(block_revenue_collector)
            } else {
                None
            }
        };

        let node_pubkey = solana_pubkey::new_rand();
        let instruction_data = serialize(&VoteInstruction::UpdateValidatorIdentity).unwrap();
        let transaction_accounts = vec![
            (vote_pubkey, vote_account),
            (node_pubkey, AccountSharedData::default()),
            (authorized_withdrawer, AccountSharedData::default()),
        ];
        let mut instruction_accounts = vec![
            AccountMeta {
                pubkey: vote_pubkey,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: node_pubkey,
                is_signer: true,
                is_writable: false,
            },
            AccountMeta {
                pubkey: authorized_withdrawer,
                is_signer: true,
                is_writable: false,
            },
        ];

        let features = VoteProgramFeatures {
            vote_state_v4,
            custom_commission_collector,
            ..Default::default()
        };

        // should fail, node_pubkey didn't sign the transaction
        instruction_accounts[1].is_signer = false;
        let accounts = process_instruction(
            features,
            &instruction_data,
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Err(InstructionError::MissingRequiredSignature),
        );
        instruction_accounts[1].is_signer = true;
        let vote_state =
            deserialize_vote_state_for_test(vote_state_v4, accounts[0].data(), &vote_pubkey);
        assert_ne!(*vote_state.node_pubkey(), node_pubkey);

        // should fail, authorized_withdrawer didn't sign the transaction
        instruction_accounts[2].is_signer = false;
        let accounts = process_instruction(
            features,
            &instruction_data,
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Err(InstructionError::MissingRequiredSignature),
        );
        instruction_accounts[2].is_signer = true;
        let vote_state =
            deserialize_vote_state_for_test(vote_state_v4, accounts[0].data(), &vote_pubkey);
        assert_ne!(*vote_state.node_pubkey(), node_pubkey);

        // should pass
        let accounts = process_instruction(
            features,
            &instruction_data,
            transaction_accounts,
            instruction_accounts,
            Ok(()),
        );
        let vote_state =
            deserialize_vote_state_for_test(vote_state_v4, accounts[0].data(), &vote_pubkey);
        assert_eq!(*vote_state.node_pubkey(), node_pubkey);
        if vote_state_v4 {
            if custom_commission_collector {
                // If SIMD-0232 is enabled, block revenue collector should be
                // unchanged.
                let original_block_revenue_collector = original_block_revenue_collector.unwrap();
                assert_eq!(
                    vote_state.as_ref_v4().block_revenue_collector,
                    original_block_revenue_collector,
                );
            } else {
                // If SIMD-0232 is disabled, block revenue collector should be
                // synced with identity.
                assert_eq!(vote_state.as_ref_v4().block_revenue_collector, node_pubkey);
            }
        }
    }

    #[test_case(false ; "VoteStateV3")]
    #[test_case(true ; "VoteStateV4")]
    fn test_vote_update_commission(vote_state_v4: bool) {
        let (vote_pubkey, _authorized_voter, authorized_withdrawer, vote_account) =
            create_test_account_with_authorized(vote_state_v4);
        let instruction_data = serialize(&VoteInstruction::UpdateCommission(42)).unwrap();
        let transaction_accounts = vec![
            (vote_pubkey, vote_account),
            (authorized_withdrawer, AccountSharedData::default()),
            // Add the sysvar accounts so they're in the cache for mock processing
            (
                sysvar::clock::id(),
                account::create_account_shared_data_for_test(&Clock::default()),
            ),
            (
                sysvar::epoch_schedule::id(),
                account::create_account_shared_data_for_test(&EpochSchedule::without_warmup()),
            ),
        ];
        let mut instruction_accounts = vec![
            AccountMeta {
                pubkey: vote_pubkey,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: authorized_withdrawer,
                is_signer: true,
                is_writable: false,
            },
        ];

        let features = VoteProgramFeatures {
            vote_state_v4,
            ..Default::default()
        };

        // should pass
        let accounts = process_instruction(
            features,
            &serialize(&VoteInstruction::UpdateCommission(200)).unwrap(),
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Ok(()),
        );
        let vote_state =
            deserialize_vote_state_for_test(vote_state_v4, accounts[0].data(), &vote_pubkey);
        assert_eq!(vote_state.commission(), 200);

        // should pass
        let accounts = process_instruction(
            features,
            &instruction_data,
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Ok(()),
        );
        let vote_state =
            deserialize_vote_state_for_test(vote_state_v4, accounts[0].data(), &vote_pubkey);
        assert_eq!(vote_state.commission(), 42);

        // should fail, authorized_withdrawer didn't sign the transaction
        instruction_accounts[1].is_signer = false;
        let accounts = process_instruction(
            features,
            &instruction_data,
            transaction_accounts,
            instruction_accounts,
            Err(InstructionError::MissingRequiredSignature),
        );
        let vote_state =
            deserialize_vote_state_for_test(vote_state_v4, accounts[0].data(), &vote_pubkey);
        assert_eq!(vote_state.commission(), 0);
    }

    #[test]
    fn test_vote_update_commission_bps() {
        // Test UpdateCommissionBps instruction (SIMD-0291).
        // SIMD-0291 depends on vote_state_v4, so we only test with V4.
        let vote_state_v4 = true;
        let commission_rate_in_basis_points = true;

        let (vote_pubkey, _authorized_voter, authorized_withdrawer, vote_account) =
            create_test_account_with_authorized(vote_state_v4);

        let transaction_accounts = vec![
            (vote_pubkey, vote_account.clone()),
            (authorized_withdrawer, AccountSharedData::default()),
        ];

        let instruction_accounts = vec![
            AccountMeta {
                pubkey: vote_pubkey,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: authorized_withdrawer,
                is_signer: true,
                is_writable: false,
            },
        ];

        let features = VoteProgramFeatures::all_enabled();

        let get_commission_bps = |vote_account: &AccountSharedData, kind: &CommissionKind| {
            let vote_state =
                deserialize_vote_state_for_test(vote_state_v4, vote_account.data(), &vote_pubkey);
            match kind {
                CommissionKind::InflationRewards => {
                    vote_state.as_ref_v4().inflation_rewards_commission_bps
                }
                CommissionKind::BlockRevenue => vote_state.as_ref_v4().block_revenue_commission_bps,
            }
        };

        let original_commission_bps =
            get_commission_bps(&vote_account, &CommissionKind::InflationRewards);

        let commission_bps = 200; // 2%

        for kind in [
            CommissionKind::InflationRewards,
            CommissionKind::BlockRevenue,
        ] {
            let other_kind = match kind {
                CommissionKind::InflationRewards => CommissionKind::BlockRevenue,
                CommissionKind::BlockRevenue => CommissionKind::InflationRewards,
            };

            // Get the original commission for the other kind.
            let original_other_commission_bps = get_commission_bps(&vote_account, &other_kind);

            let instruction_data = serialize(&VoteInstruction::UpdateCommissionBps {
                commission_bps,
                kind: kind.clone(),
            })
            .unwrap();

            // Should pass.
            let accounts = process_instruction(
                features,
                &instruction_data,
                transaction_accounts.clone(),
                instruction_accounts.clone(),
                Ok(()),
            );
            assert_eq!(get_commission_bps(&accounts[0], &kind), commission_bps);

            // Verify the other commission kind was not affected.
            assert_eq!(
                get_commission_bps(&accounts[0], &other_kind),
                original_other_commission_bps,
            );

            // Same value - should pass.
            let accounts = process_instruction(
                features,
                &instruction_data,
                vec![
                    (vote_pubkey, accounts[0].clone()),
                    (authorized_withdrawer, accounts[1].clone()),
                ],
                instruction_accounts.clone(),
                Ok(()),
            );
            assert_eq!(get_commission_bps(&accounts[0], &kind), commission_bps);

            // Verify the other commission kind is still unchanged.
            assert_eq!(
                get_commission_bps(&accounts[0], &other_kind),
                original_other_commission_bps,
            );
        }

        let instruction_data = serialize(&VoteInstruction::UpdateCommissionBps {
            commission_bps,
            kind: CommissionKind::InflationRewards,
        })
        .unwrap();

        // Should fail - SIMD-0185 disabled.
        let accounts = process_instruction(
            VoteProgramFeatures {
                vote_state_v4: false,
                commission_rate_in_basis_points,
                ..Default::default()
            },
            &instruction_data,
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Err(InstructionError::InvalidInstructionData),
        );
        let stored_commission_bps =
            get_commission_bps(&accounts[0], &CommissionKind::InflationRewards);
        assert_eq!(stored_commission_bps, original_commission_bps); // Matches original
        assert_ne!(stored_commission_bps, commission_bps); // New value not set

        // Should fail - `CommissionKind::BlockRevenue` disallowed (SIMD-0123 disabled).
        let accounts = process_instruction(
            VoteProgramFeatures {
                block_revenue_sharing: false,
                ..features
            },
            &serialize(&VoteInstruction::UpdateCommissionBps {
                commission_bps,
                kind: CommissionKind::BlockRevenue,
            })
            .unwrap(),
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Err(InstructionError::InvalidInstructionData),
        );
        let stored_commission_bps = get_commission_bps(&accounts[0], &CommissionKind::BlockRevenue);
        assert_eq!(stored_commission_bps, 0); // BlockRevenue starts at 0
        assert_ne!(stored_commission_bps, commission_bps); // New value not set

        // Should fail - authorized withdrawer didn't sign the transaction.
        let mut unsigned_instruction_accounts = instruction_accounts.clone();
        unsigned_instruction_accounts[1].is_signer = false;
        let accounts = process_instruction(
            features,
            &instruction_data,
            transaction_accounts.clone(),
            unsigned_instruction_accounts,
            Err(InstructionError::MissingRequiredSignature),
        );
        let stored_commission_bps =
            get_commission_bps(&accounts[0], &CommissionKind::InflationRewards);
        assert_eq!(stored_commission_bps, original_commission_bps); // Matches original
        assert_ne!(stored_commission_bps, commission_bps); // New value not set

        // Should fail - wrong signature for authorized withdrawer.
        let wrong_signer = Pubkey::new_unique();
        let mut wrong_signer_transaction_accounts = transaction_accounts.clone();
        wrong_signer_transaction_accounts.push((wrong_signer, AccountSharedData::default()));
        let wrong_signer_instruction_accounts = vec![
            AccountMeta {
                pubkey: vote_pubkey,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: wrong_signer,
                is_signer: true,
                is_writable: false,
            },
        ];
        let accounts = process_instruction(
            features,
            &instruction_data,
            wrong_signer_transaction_accounts,
            wrong_signer_instruction_accounts,
            Err(InstructionError::MissingRequiredSignature),
        );
        let stored_commission_bps =
            get_commission_bps(&accounts[0], &CommissionKind::InflationRewards);
        assert_eq!(stored_commission_bps, original_commission_bps); // Matches original
        assert_ne!(stored_commission_bps, commission_bps); // New value not set
    }

    #[test]
    fn test_vote_update_commission_collector() {
        // Test UpdateCommissionCollector instruction (SIMD-0232).
        // SIMD-0232 depends on vote_state_v4, so we only test with V4.
        let vote_state_v4 = true;
        let custom_commission_collector = true;

        let (vote_pubkey, _authorized_voter, authorized_withdrawer, vote_account) =
            create_test_account_with_authorized(vote_state_v4);

        // Create a valid collector account: system-owned and rent-exempt.
        let new_collector_pubkey = Pubkey::new_unique();
        let rent = Rent::default();
        let rent_sysvar_account = account::create_account_shared_data_for_test(&rent);
        let collector_lamports = rent.minimum_balance(0);
        let new_collector_account =
            AccountSharedData::new(collector_lamports, 0, &solana_sdk_ids::system_program::id());

        let transaction_accounts = vec![
            (vote_pubkey, vote_account.clone()),
            (new_collector_pubkey, new_collector_account.clone()),
            (authorized_withdrawer, AccountSharedData::default()),
            (sysvar::rent::id(), rent_sysvar_account),
        ];

        let instruction_accounts = vec![
            AccountMeta {
                pubkey: vote_pubkey,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: new_collector_pubkey,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: authorized_withdrawer,
                is_signer: true,
                is_writable: false,
            },
        ];

        let features = VoteProgramFeatures {
            vote_state_v4,
            custom_commission_collector,
            ..Default::default()
        };

        let get_commission_collector = |vote_account: &AccountSharedData, kind: CommissionKind| {
            let vote_state =
                deserialize_vote_state_for_test(vote_state_v4, vote_account.data(), &vote_pubkey)
                    .as_ref_v4()
                    .clone();
            match kind {
                CommissionKind::InflationRewards => vote_state.inflation_rewards_collector,
                CommissionKind::BlockRevenue => vote_state.block_revenue_collector,
            }
        };

        let original_inflation_collector =
            get_commission_collector(&vote_account, CommissionKind::InflationRewards);
        let original_block_revenue_collector =
            get_commission_collector(&vote_account, CommissionKind::BlockRevenue);

        // Should pass - InflationRewards kind.
        let instruction_data = serialize(&VoteInstruction::UpdateCommissionCollector(
            CommissionKind::InflationRewards,
        ))
        .unwrap();
        let accounts = process_instruction(
            features,
            &instruction_data,
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Ok(()),
        );
        assert_eq!(
            get_commission_collector(&accounts[0], CommissionKind::InflationRewards),
            new_collector_pubkey,
        );
        assert_eq!(
            get_commission_collector(&accounts[0], CommissionKind::BlockRevenue),
            original_block_revenue_collector, // Unchanged
        );

        // Should pass - BlockRevenue kind.
        let instruction_data = serialize(&VoteInstruction::UpdateCommissionCollector(
            CommissionKind::BlockRevenue,
        ))
        .unwrap();
        let accounts = process_instruction(
            features,
            &instruction_data,
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Ok(()),
        );
        assert_eq!(
            get_commission_collector(&accounts[0], CommissionKind::InflationRewards),
            original_inflation_collector, // Unchanged
        );
        assert_eq!(
            get_commission_collector(&accounts[0], CommissionKind::BlockRevenue),
            new_collector_pubkey,
        );

        // Should pass - setting collector to vote account (InflationRewards).
        let vote_as_collector_instruction_accounts = vec![
            AccountMeta {
                pubkey: vote_pubkey,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: vote_pubkey, // Collector is the vote account.
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: authorized_withdrawer,
                is_signer: true,
                is_writable: false,
            },
        ];
        let instruction_data = serialize(&VoteInstruction::UpdateCommissionCollector(
            CommissionKind::InflationRewards,
        ))
        .unwrap();
        let accounts = process_instruction(
            features,
            &instruction_data,
            transaction_accounts.clone(),
            vote_as_collector_instruction_accounts.clone(),
            Ok(()),
        );
        assert_eq!(
            get_commission_collector(&accounts[0], CommissionKind::InflationRewards),
            vote_pubkey
        );
        assert_eq!(
            get_commission_collector(&accounts[0], CommissionKind::BlockRevenue),
            original_block_revenue_collector, // Unchanged
        );

        // Should pass - setting collector to vote account (BlockRevenue).
        let instruction_data = serialize(&VoteInstruction::UpdateCommissionCollector(
            CommissionKind::BlockRevenue,
        ))
        .unwrap();
        let accounts = process_instruction(
            features,
            &instruction_data,
            transaction_accounts.clone(),
            vote_as_collector_instruction_accounts,
            Ok(()),
        );
        assert_eq!(
            get_commission_collector(&accounts[0], CommissionKind::InflationRewards),
            original_inflation_collector, // Unchanged
        );
        assert_eq!(
            get_commission_collector(&accounts[0], CommissionKind::BlockRevenue),
            vote_pubkey
        );

        // Should fail - SIMD-0232 disabled.
        let instruction_data = serialize(&VoteInstruction::UpdateCommissionCollector(
            CommissionKind::InflationRewards,
        ))
        .unwrap();
        let accounts = process_instruction(
            VoteProgramFeatures {
                vote_state_v4,
                custom_commission_collector: false,
                ..Default::default()
            },
            &instruction_data,
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Err(InstructionError::InvalidInstructionData),
        );
        assert_eq!(
            get_commission_collector(&accounts[0], CommissionKind::InflationRewards),
            original_inflation_collector
        );
        assert_eq!(
            get_commission_collector(&accounts[0], CommissionKind::BlockRevenue),
            original_block_revenue_collector, // Unchanged
        );

        // Should fail - SIMD-0185 (vote_state_v4) disabled.
        let accounts = process_instruction(
            VoteProgramFeatures {
                vote_state_v4: false,
                custom_commission_collector,
                ..Default::default()
            },
            &instruction_data,
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Err(InstructionError::InvalidInstructionData),
        );
        assert_eq!(
            get_commission_collector(&accounts[0], CommissionKind::InflationRewards),
            original_inflation_collector
        );
        assert_eq!(
            get_commission_collector(&accounts[0], CommissionKind::BlockRevenue),
            original_block_revenue_collector, // Unchanged
        );

        // Should fail - authorized withdrawer didn't sign.
        let mut unsigned_instruction_accounts = instruction_accounts.clone();
        unsigned_instruction_accounts[2].is_signer = false;
        let accounts = process_instruction(
            features,
            &instruction_data,
            transaction_accounts.clone(),
            unsigned_instruction_accounts,
            Err(InstructionError::MissingRequiredSignature),
        );
        assert_eq!(
            get_commission_collector(&accounts[0], CommissionKind::InflationRewards),
            original_inflation_collector
        );
        assert_eq!(
            get_commission_collector(&accounts[0], CommissionKind::BlockRevenue),
            original_block_revenue_collector, // Unchanged
        );

        // Should fail - wrong signer (not the authorized withdrawer).
        let wrong_signer = Pubkey::new_unique();
        let mut wrong_signer_transaction_accounts = transaction_accounts.clone();
        wrong_signer_transaction_accounts.push((wrong_signer, AccountSharedData::default()));
        let wrong_signer_instruction_accounts = vec![
            AccountMeta {
                pubkey: vote_pubkey,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: new_collector_pubkey,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: wrong_signer,
                is_signer: true,
                is_writable: false,
            },
        ];
        let accounts = process_instruction(
            features,
            &instruction_data,
            wrong_signer_transaction_accounts,
            wrong_signer_instruction_accounts,
            Err(InstructionError::MissingRequiredSignature),
        );
        assert_eq!(
            get_commission_collector(&accounts[0], CommissionKind::InflationRewards),
            original_inflation_collector
        );
        assert_eq!(
            get_commission_collector(&accounts[0], CommissionKind::BlockRevenue),
            original_block_revenue_collector, // Unchanged
        );

        // Should fail - new collector not system program owned.
        let non_system_owner = Pubkey::new_unique();
        let non_system_collector_pubkey = Pubkey::new_unique();
        let non_system_collector_account =
            AccountSharedData::new(collector_lamports, 0, &non_system_owner);
        let mut non_system_transaction_accounts = transaction_accounts.clone();
        non_system_transaction_accounts[1] =
            (non_system_collector_pubkey, non_system_collector_account);
        let non_system_instruction_accounts = vec![
            AccountMeta {
                pubkey: vote_pubkey,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: non_system_collector_pubkey,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: authorized_withdrawer,
                is_signer: true,
                is_writable: false,
            },
        ];
        let accounts = process_instruction(
            features,
            &instruction_data,
            non_system_transaction_accounts,
            non_system_instruction_accounts,
            Err(InstructionError::InvalidAccountOwner),
        );
        assert_eq!(
            get_commission_collector(&accounts[0], CommissionKind::InflationRewards),
            original_inflation_collector
        );
        assert_eq!(
            get_commission_collector(&accounts[0], CommissionKind::BlockRevenue),
            original_block_revenue_collector, // Unchanged
        );

        // Should fail - new collector not rent-exempt.
        let not_rent_exempt_collector_pubkey = Pubkey::new_unique();
        let not_rent_exempt_collector_account =
            AccountSharedData::new(0, 0, &solana_sdk_ids::system_program::id()); // 0 lamports
        let mut not_rent_exempt_transaction_accounts = transaction_accounts.clone();
        not_rent_exempt_transaction_accounts[1] = (
            not_rent_exempt_collector_pubkey,
            not_rent_exempt_collector_account,
        );
        let not_rent_exempt_instruction_accounts = vec![
            AccountMeta {
                pubkey: vote_pubkey,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: not_rent_exempt_collector_pubkey,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: authorized_withdrawer,
                is_signer: true,
                is_writable: false,
            },
        ];
        let accounts = process_instruction(
            features,
            &instruction_data,
            not_rent_exempt_transaction_accounts,
            not_rent_exempt_instruction_accounts,
            Err(InstructionError::InsufficientFunds),
        );
        assert_eq!(
            get_commission_collector(&accounts[0], CommissionKind::InflationRewards),
            original_inflation_collector
        );
        assert_eq!(
            get_commission_collector(&accounts[0], CommissionKind::BlockRevenue),
            original_block_revenue_collector, // Unchanged
        );

        // Should fail - new collector not writable (reserved account check).
        let mut not_writable_instruction_accounts = instruction_accounts.clone();
        not_writable_instruction_accounts[1].is_writable = false;
        let accounts = process_instruction(
            features,
            &instruction_data,
            transaction_accounts.clone(),
            not_writable_instruction_accounts,
            Err(InstructionError::InvalidArgument),
        );
        assert_eq!(
            get_commission_collector(&accounts[0], CommissionKind::InflationRewards),
            original_inflation_collector
        );
        assert_eq!(
            get_commission_collector(&accounts[0], CommissionKind::BlockRevenue),
            original_block_revenue_collector, // Unchanged
        );
    }

    #[test_case(false ; "VoteStateV3")]
    #[test_case(true ; "VoteStateV4")]
    fn test_vote_signature(vote_state_v4: bool) {
        let (vote_pubkey, vote_account) = create_test_account(vote_state_v4);
        let (vote, instruction_datas) = create_serialized_votes();
        let slot_hashes = SlotHashes::new(&[(*vote.slots.last().unwrap(), vote.hash)]);
        let slot_hashes_account = account::create_account_shared_data_for_test(&slot_hashes);
        let mut instruction_accounts = vec![
            AccountMeta {
                pubkey: vote_pubkey,
                is_signer: true,
                is_writable: true,
            },
            AccountMeta {
                pubkey: sysvar::slot_hashes::id(),
                is_signer: false,
                is_writable: false,
            },
            AccountMeta {
                pubkey: sysvar::clock::id(),
                is_signer: false,
                is_writable: false,
            },
        ];

        let features = VoteProgramFeatures {
            vote_state_v4,
            ..Default::default()
        };

        for (instruction_data, is_tower_sync) in instruction_datas {
            let mut transaction_accounts = vec![
                (vote_pubkey, vote_account.clone()),
                (sysvar::slot_hashes::id(), slot_hashes_account.clone()),
                (sysvar::clock::id(), create_default_clock_account()),
            ];

            let error = |err| {
                if !is_tower_sync {
                    Err(InstructionError::InvalidInstructionData)
                } else {
                    Err(err)
                }
            };

            // should fail, unsigned
            instruction_accounts[0].is_signer = false;
            process_instruction(
                features,
                &instruction_data,
                transaction_accounts.clone(),
                instruction_accounts.clone(),
                error(InstructionError::MissingRequiredSignature),
            );
            instruction_accounts[0].is_signer = true;

            // should pass
            let accounts = process_instruction(
                features,
                &instruction_data,
                transaction_accounts.clone(),
                instruction_accounts.clone(),
                if is_tower_sync {
                    Ok(())
                } else {
                    Err(InstructionError::InvalidInstructionData)
                },
            );
            if is_tower_sync {
                let vote_state = deserialize_vote_state_for_test(
                    vote_state_v4,
                    accounts[0].data(),
                    &vote_pubkey,
                );
                let expected_lockout = Lockout::new(*vote.slots.last().unwrap());
                assert_eq!(vote_state.votes().len(), 1);
                assert_eq!(vote_state.votes()[0].lockout, expected_lockout);
                assert_eq!(vote_state.credits(), 0);
            }

            // should fail, wrong hash
            transaction_accounts[1] = (
                sysvar::slot_hashes::id(),
                account::create_account_shared_data_for_test(&SlotHashes::new(&[(
                    *vote.slots.last().unwrap(),
                    solana_sha256_hasher::hash(&[0u8]),
                )])),
            );
            process_instruction(
                features,
                &instruction_data,
                transaction_accounts.clone(),
                instruction_accounts.clone(),
                error(VoteError::SlotHashMismatch.into()),
            );

            // should fail, wrong slot
            transaction_accounts[1] = (
                sysvar::slot_hashes::id(),
                account::create_account_shared_data_for_test(&SlotHashes::new(&[(0, vote.hash)])),
            );
            process_instruction(
                features,
                &instruction_data,
                transaction_accounts.clone(),
                instruction_accounts.clone(),
                error(VoteError::SlotsMismatch.into()),
            );

            // should fail, empty slot_hashes
            transaction_accounts[1] = (
                sysvar::slot_hashes::id(),
                account::create_account_shared_data_for_test(&SlotHashes::new(&[])),
            );
            process_instruction(
                features,
                &instruction_data,
                transaction_accounts.clone(),
                instruction_accounts.clone(),
                error(VoteError::SlotsMismatch.into()),
            );
            transaction_accounts[1] = (sysvar::slot_hashes::id(), slot_hashes_account.clone());

            // should fail, uninitialized
            let vote_account =
                AccountSharedData::new(100, vote_state_size_of(vote_state_v4), &id());
            transaction_accounts[0] = (vote_pubkey, vote_account);
            process_instruction(
                features,
                &instruction_data,
                transaction_accounts.clone(),
                instruction_accounts.clone(),
                if vote_state_v4 {
                    error(InstructionError::UninitializedAccount)
                } else {
                    error(InstructionError::InvalidAccountData)
                },
            );
        }
    }

    #[test_matrix([false, true], [false, true])]
    fn test_authorize_voter(vote_state_v4: bool, bls_pubkey_management_in_vote_account: bool) {
        let (vote_pubkey, vote_account) = create_test_account(vote_state_v4);
        let authorized_voter_pubkey = solana_pubkey::new_rand();
        let clock = Clock {
            epoch: 1,
            leader_schedule_epoch: 2,
            ..Clock::default()
        };
        let clock_account = account::create_account_shared_data_for_test(&clock);
        let instruction_data = serialize(&VoteInstruction::Authorize(
            authorized_voter_pubkey,
            VoteAuthorize::Voter,
        ))
        .unwrap();

        let mut transaction_accounts = vec![
            (vote_pubkey, vote_account.clone()),
            (sysvar::clock::id(), clock_account.clone()),
            (authorized_voter_pubkey, AccountSharedData::default()),
        ];
        let mut instruction_accounts = vec![
            AccountMeta {
                pubkey: vote_pubkey,
                is_signer: true,
                is_writable: true,
            },
            AccountMeta {
                pubkey: sysvar::clock::id(),
                is_signer: false,
                is_writable: false,
            },
        ];

        let features = VoteProgramFeatures {
            vote_state_v4,
            bls_pubkey_management_in_vote_account,
            ..Default::default()
        };

        // processing incompatible instruction should fail
        if vote_state_v4 && bls_pubkey_management_in_vote_account {
            // If both features are enabled, the old instruction should be rejected
            process_instruction(
                features,
                &instruction_data,
                vec![
                    (vote_pubkey, vote_account.clone()),
                    (sysvar::clock::id(), clock_account.clone()),
                    (authorized_voter_pubkey, AccountSharedData::default()),
                ],
                instruction_accounts.clone(),
                Err(InstructionError::InvalidInstructionData),
            );
            return;
        } else {
            // If either feature is disabled, the new instruction should be rejected
            let (bls_pubkey, bls_proof_of_possession) =
                create_bls_pubkey_and_proof_of_possession(&vote_pubkey);
            let bad_instruction_data = serialize(&VoteInstruction::Authorize(
                authorized_voter_pubkey,
                VoteAuthorize::VoterWithBLS(VoterWithBLSArgs {
                    bls_pubkey,
                    bls_proof_of_possession,
                }),
            ))
            .unwrap();
            process_instruction(
                features,
                &bad_instruction_data,
                vec![
                    (vote_pubkey, vote_account.clone()),
                    (sysvar::clock::id(), clock_account.clone()),
                    (authorized_voter_pubkey, AccountSharedData::default()),
                ],
                instruction_accounts.clone(),
                Err(InstructionError::InvalidInstructionData),
            );
        }

        // should fail, unsigned
        instruction_accounts[0].is_signer = false;
        process_instruction(
            features,
            &instruction_data,
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Err(InstructionError::MissingRequiredSignature),
        );
        instruction_accounts[0].is_signer = true;

        // should pass
        let accounts = process_instruction(
            features,
            &instruction_data,
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Ok(()),
        );

        // should fail, already set an authorized voter earlier for leader_schedule_epoch == 2
        transaction_accounts[0] = (vote_pubkey, accounts[0].clone());
        process_instruction(
            features,
            &instruction_data,
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Err(VoteError::TooSoonToReauthorize.into()),
        );

        // should pass, verify authorized_voter_pubkey can authorize authorized_voter_pubkey ;)
        instruction_accounts[0].is_signer = false;
        instruction_accounts.push(AccountMeta {
            pubkey: authorized_voter_pubkey,
            is_signer: true,
            is_writable: false,
        });
        let clock = Clock {
            // The authorized voter was set when leader_schedule_epoch == 2, so will
            // take effect when epoch == 3
            epoch: 3,
            leader_schedule_epoch: 4,
            ..Clock::default()
        };
        let clock_account = account::create_account_shared_data_for_test(&clock);
        transaction_accounts[1] = (sysvar::clock::id(), clock_account);
        process_instruction(
            features,
            &instruction_data,
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Ok(()),
        );
        instruction_accounts[0].is_signer = true;
        instruction_accounts.pop();

        // should fail, not signed by authorized voter
        let (vote, instruction_datas) = create_serialized_votes();
        let slot_hashes = SlotHashes::new(&[(*vote.slots.last().unwrap(), vote.hash)]);
        let slot_hashes_account = account::create_account_shared_data_for_test(&slot_hashes);
        transaction_accounts.push((sysvar::slot_hashes::id(), slot_hashes_account));
        instruction_accounts.insert(
            1,
            AccountMeta {
                pubkey: sysvar::slot_hashes::id(),
                is_signer: false,
                is_writable: false,
            },
        );
        let mut authorized_instruction_accounts = instruction_accounts.clone();
        authorized_instruction_accounts.push(AccountMeta {
            pubkey: authorized_voter_pubkey,
            is_signer: true,
            is_writable: false,
        });

        for (instruction_data, is_tower_sync) in instruction_datas {
            process_instruction(
                features,
                &instruction_data,
                transaction_accounts.clone(),
                instruction_accounts.clone(),
                Err(if is_tower_sync {
                    InstructionError::MissingRequiredSignature
                } else {
                    InstructionError::InvalidInstructionData
                }),
            );

            // should pass, signed by authorized voter
            process_instruction(
                features,
                &instruction_data,
                transaction_accounts.clone(),
                authorized_instruction_accounts.clone(),
                if is_tower_sync {
                    Ok(())
                } else {
                    Err(InstructionError::InvalidInstructionData)
                },
            );
        }
    }

    #[test_matrix([false, true], [false, true])]
    fn test_authorize_voter_with_bls(
        vote_state_v4: bool,
        bls_pubkey_management_in_vote_account: bool,
    ) {
        agave_logger::setup();
        let (vote_pubkey, vote_account) = create_test_account(vote_state_v4);
        let authorized_voter_pubkey = solana_pubkey::new_rand();
        let clock = Clock {
            epoch: 1,
            leader_schedule_epoch: 2,
            ..Clock::default()
        };
        let clock_account = account::create_account_shared_data_for_test(&clock);
        let (bls_pubkey, bls_proof_of_possession) =
            create_bls_pubkey_and_proof_of_possession(&vote_pubkey);
        let instruction_data = serialize(&VoteInstruction::Authorize(
            authorized_voter_pubkey,
            VoteAuthorize::VoterWithBLS(VoterWithBLSArgs {
                bls_pubkey,
                bls_proof_of_possession,
            }),
        ))
        .unwrap();

        let mut transaction_accounts = vec![
            (vote_pubkey, vote_account.clone()),
            (sysvar::clock::id(), clock_account.clone()),
            (authorized_voter_pubkey, AccountSharedData::default()),
        ];
        let mut instruction_accounts = vec![
            AccountMeta {
                pubkey: vote_pubkey,
                is_signer: true,
                is_writable: true,
            },
            AccountMeta {
                pubkey: sysvar::clock::id(),
                is_signer: false,
                is_writable: false,
            },
        ];

        let features = VoteProgramFeatures {
            vote_state_v4,
            bls_pubkey_management_in_vote_account,
            ..Default::default()
        };

        // processing incompatible instruction should fail
        if vote_state_v4 && bls_pubkey_management_in_vote_account {
            // If both features are enabled, the old instruction should be accepted when
            // the account does not have a BLS key.
            let (new_vote_pubkey, vote_account_no_bls_key) = create_test_account(false);
            let new_authorized_voter_pubkey = solana_pubkey::new_rand();
            let old_instruction_data = serialize(&VoteInstruction::Authorize(
                new_authorized_voter_pubkey,
                VoteAuthorize::Voter,
            ))
            .unwrap();
            process_instruction(
                features,
                &old_instruction_data,
                vec![
                    (new_vote_pubkey, vote_account_no_bls_key),
                    (sysvar::clock::id(), clock_account.clone()),
                    (new_authorized_voter_pubkey, AccountSharedData::default()),
                ],
                vec![
                    AccountMeta {
                        pubkey: new_vote_pubkey,
                        is_signer: true,
                        is_writable: true,
                    },
                    AccountMeta {
                        pubkey: sysvar::clock::id(),
                        is_signer: false,
                        is_writable: false,
                    },
                ],
                Ok(()),
            );
            // However, once the BLS key is set, the old instruction should be rejected
            let (new_vote_pubkey, vote_account_with_bls_key) = create_test_account(true);
            let new_authorized_voter_pubkey = solana_pubkey::new_rand();
            let old_instruction_data = serialize(&VoteInstruction::Authorize(
                new_authorized_voter_pubkey,
                VoteAuthorize::Voter,
            ))
            .unwrap();
            process_instruction(
                features,
                &old_instruction_data,
                vec![
                    (new_vote_pubkey, vote_account_with_bls_key),
                    (sysvar::clock::id(), clock_account.clone()),
                    (new_authorized_voter_pubkey, AccountSharedData::default()),
                ],
                vec![
                    AccountMeta {
                        pubkey: new_vote_pubkey,
                        is_signer: true,
                        is_writable: true,
                    },
                    AccountMeta {
                        pubkey: sysvar::clock::id(),
                        is_signer: false,
                        is_writable: false,
                    },
                ],
                Err(InstructionError::InvalidInstructionData),
            );
        } else {
            // If either feature is disabled, the new instruction should be rejected
            let (bls_pubkey, bls_proof_of_possession) =
                create_bls_pubkey_and_proof_of_possession(&vote_pubkey);
            let bad_instruction_data = serialize(&VoteInstruction::Authorize(
                authorized_voter_pubkey,
                VoteAuthorize::VoterWithBLS(VoterWithBLSArgs {
                    bls_pubkey,
                    bls_proof_of_possession,
                }),
            ))
            .unwrap();

            process_instruction(
                features,
                &bad_instruction_data,
                vec![
                    (vote_pubkey, vote_account.clone()),
                    (sysvar::clock::id(), clock_account.clone()),
                    (authorized_voter_pubkey, AccountSharedData::default()),
                ],
                instruction_accounts.clone(),
                Err(InstructionError::InvalidInstructionData),
            );
            return;
        }

        // should fail, unsigned
        instruction_accounts[0].is_signer = false;
        process_instruction_with_cu_check(
            features,
            &instruction_data,
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Err(InstructionError::MissingRequiredSignature),
            DEFAULT_COMPUTE_UNITS + BLS_PROOF_OF_POSSESSION_VERIFICATION_COMPUTE_UNITS,
        );
        instruction_accounts[0].is_signer = true;

        // should pass
        let accounts = process_instruction_with_cu_check(
            features,
            &instruction_data,
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Ok(()),
            DEFAULT_COMPUTE_UNITS + BLS_PROOF_OF_POSSESSION_VERIFICATION_COMPUTE_UNITS,
        );

        // should fail, already set an authorized voter earlier for leader_schedule_epoch == 2
        transaction_accounts[0] = (vote_pubkey, accounts[0].clone());
        process_instruction_with_cu_check(
            features,
            &instruction_data,
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Err(VoteError::TooSoonToReauthorize.into()),
            DEFAULT_COMPUTE_UNITS + BLS_PROOF_OF_POSSESSION_VERIFICATION_COMPUTE_UNITS,
        );

        // should pass, verify authorized_voter_pubkey can authorize authorized_voter_pubkey ;)
        instruction_accounts[0].is_signer = false;
        instruction_accounts.push(AccountMeta {
            pubkey: authorized_voter_pubkey,
            is_signer: true,
            is_writable: false,
        });
        let clock = Clock {
            // The authorized voter was set when leader_schedule_epoch == 2, so will
            // take effect when epoch == 3
            epoch: 3,
            leader_schedule_epoch: 4,
            ..Clock::default()
        };
        let clock_account = account::create_account_shared_data_for_test(&clock);
        transaction_accounts[1] = (sysvar::clock::id(), clock_account);
        process_instruction_with_cu_check(
            features,
            &instruction_data,
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Ok(()),
            DEFAULT_COMPUTE_UNITS + BLS_PROOF_OF_POSSESSION_VERIFICATION_COMPUTE_UNITS,
        );
        instruction_accounts[0].is_signer = true;
        instruction_accounts.pop();

        // should fail, not signed by authorized voter
        let (vote, instruction_datas) = create_serialized_votes();
        let slot_hashes = SlotHashes::new(&[(*vote.slots.last().unwrap(), vote.hash)]);
        let slot_hashes_account = account::create_account_shared_data_for_test(&slot_hashes);
        transaction_accounts.push((sysvar::slot_hashes::id(), slot_hashes_account));
        instruction_accounts.insert(
            1,
            AccountMeta {
                pubkey: sysvar::slot_hashes::id(),
                is_signer: false,
                is_writable: false,
            },
        );
        let mut authorized_instruction_accounts = instruction_accounts.clone();
        authorized_instruction_accounts.push(AccountMeta {
            pubkey: authorized_voter_pubkey,
            is_signer: true,
            is_writable: false,
        });

        for (instruction_data, is_tower_sync) in instruction_datas {
            process_instruction(
                features,
                &instruction_data,
                transaction_accounts.clone(),
                instruction_accounts.clone(),
                Err(if is_tower_sync {
                    InstructionError::MissingRequiredSignature
                } else {
                    InstructionError::InvalidInstructionData
                }),
            );

            // should pass, signed by authorized voter
            process_instruction(
                features,
                &instruction_data,
                transaction_accounts.clone(),
                authorized_instruction_accounts.clone(),
                if is_tower_sync {
                    Ok(())
                } else {
                    Err(InstructionError::InvalidInstructionData)
                },
            );
        }
    }

    #[test]
    fn test_authorize_voter_with_bls_bad_proof_of_possession() {
        let (vote_pubkey, vote_account) = create_test_account(true);
        let authorized_voter_pubkey = solana_pubkey::new_rand();
        let clock = Clock {
            epoch: 1,
            leader_schedule_epoch: 2,
            ..Clock::default()
        };
        let clock_account = account::create_account_shared_data_for_test(&clock);
        let transaction_accounts = vec![
            (vote_pubkey, vote_account.clone()),
            (sysvar::clock::id(), clock_account.clone()),
            (authorized_voter_pubkey, AccountSharedData::default()),
        ];
        let instruction_accounts = vec![
            AccountMeta {
                pubkey: vote_pubkey,
                is_signer: true,
                is_writable: true,
            },
            AccountMeta {
                pubkey: sysvar::clock::id(),
                is_signer: false,
                is_writable: false,
            },
        ];

        // Test that bad proof of possession fails authorization
        let instruction_data = serialize(&VoteInstruction::Authorize(
            authorized_voter_pubkey,
            VoteAuthorize::VoterWithBLS(VoterWithBLSArgs {
                bls_pubkey: [1u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE],
                bls_proof_of_possession: [2u8; BLS_PROOF_OF_POSSESSION_COMPRESSED_SIZE],
            }),
        ))
        .unwrap();
        process_instruction_with_cu_check(
            VoteProgramFeatures {
                vote_state_v4: true,
                bls_pubkey_management_in_vote_account: true,
                ..Default::default()
            },
            &instruction_data,
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Err(InstructionError::InvalidArgument),
            DEFAULT_COMPUTE_UNITS + BLS_PROOF_OF_POSSESSION_VERIFICATION_COMPUTE_UNITS,
        );
    }

    #[test_case(false ; "VoteStateV3")]
    #[test_case(true ; "VoteStateV4")]
    fn test_authorize_withdrawer(vote_state_v4: bool) {
        let (vote_pubkey, vote_account) = create_test_account(vote_state_v4);
        let authorized_withdrawer_pubkey = solana_pubkey::new_rand();
        let instruction_data = serialize(&VoteInstruction::Authorize(
            authorized_withdrawer_pubkey,
            VoteAuthorize::Withdrawer,
        ))
        .unwrap();
        let mut transaction_accounts = vec![
            (vote_pubkey, vote_account),
            (sysvar::clock::id(), create_default_clock_account()),
            (authorized_withdrawer_pubkey, AccountSharedData::default()),
        ];
        let mut instruction_accounts = vec![
            AccountMeta {
                pubkey: vote_pubkey,
                is_signer: true,
                is_writable: true,
            },
            AccountMeta {
                pubkey: sysvar::clock::id(),
                is_signer: false,
                is_writable: false,
            },
        ];

        let features = VoteProgramFeatures {
            vote_state_v4,
            ..Default::default()
        };

        // should fail, unsigned
        instruction_accounts[0].is_signer = false;
        process_instruction(
            features,
            &instruction_data,
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Err(InstructionError::MissingRequiredSignature),
        );
        instruction_accounts[0].is_signer = true;

        // should pass
        let accounts = process_instruction(
            features,
            &instruction_data,
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Ok(()),
        );

        // should pass, verify authorized_withdrawer can authorize authorized_withdrawer ;)
        instruction_accounts[0].is_signer = false;
        instruction_accounts.push(AccountMeta {
            pubkey: authorized_withdrawer_pubkey,
            is_signer: true,
            is_writable: false,
        });
        transaction_accounts[0] = (vote_pubkey, accounts[0].clone());
        process_instruction(
            features,
            &instruction_data,
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Ok(()),
        );

        // should pass, verify authorized_withdrawer can authorize a new authorized_voter
        let authorized_voter_pubkey = solana_pubkey::new_rand();
        transaction_accounts.push((authorized_voter_pubkey, AccountSharedData::default()));
        let instruction_data = serialize(&VoteInstruction::Authorize(
            authorized_voter_pubkey,
            VoteAuthorize::Voter,
        ))
        .unwrap();
        process_instruction(
            features,
            &instruction_data,
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Ok(()),
        );
    }

    #[test_case(false ; "VoteStateV3")]
    #[test_case(true ; "VoteStateV4")]
    fn test_vote_withdraw(vote_state_v4: bool) {
        let (vote_pubkey, vote_account) = create_test_account(vote_state_v4);
        let lamports = vote_account.lamports();
        let authorized_withdrawer_pubkey = solana_pubkey::new_rand();
        let mut transaction_accounts = vec![
            (vote_pubkey, vote_account.clone()),
            (sysvar::clock::id(), create_default_clock_account()),
            (sysvar::rent::id(), create_default_rent_account()),
            (authorized_withdrawer_pubkey, AccountSharedData::default()),
        ];
        let mut instruction_accounts = vec![
            AccountMeta {
                pubkey: vote_pubkey,
                is_signer: true,
                is_writable: true,
            },
            AccountMeta {
                pubkey: sysvar::clock::id(),
                is_signer: false,
                is_writable: false,
            },
        ];

        let features = VoteProgramFeatures {
            vote_state_v4,
            ..Default::default()
        };

        // should pass, withdraw using authorized_withdrawer to authorized_withdrawer's account
        let accounts = process_instruction(
            features,
            &serialize(&VoteInstruction::Authorize(
                authorized_withdrawer_pubkey,
                VoteAuthorize::Withdrawer,
            ))
            .unwrap(),
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Ok(()),
        );
        instruction_accounts[0].is_signer = false;
        instruction_accounts[1] = AccountMeta {
            pubkey: authorized_withdrawer_pubkey,
            is_signer: true,
            is_writable: true,
        };
        transaction_accounts[0] = (vote_pubkey, accounts[0].clone());
        let accounts = process_instruction(
            features,
            &serialize(&VoteInstruction::Withdraw(lamports)).unwrap(),
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Ok(()),
        );
        assert_eq!(accounts[0].lamports(), 0);
        assert_eq!(accounts[3].lamports(), lamports);
        let post_state: VoteStateVersions = accounts[0].state().unwrap();
        // State has been deinitialized since balance is zero
        assert!(post_state.is_uninitialized());

        // should fail, unsigned
        transaction_accounts[0] = (vote_pubkey, vote_account);
        process_instruction(
            features,
            &serialize(&VoteInstruction::Withdraw(lamports)).unwrap(),
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Err(InstructionError::MissingRequiredSignature),
        );
        instruction_accounts[0].is_signer = true;

        // should pass
        process_instruction(
            features,
            &serialize(&VoteInstruction::Withdraw(lamports)).unwrap(),
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Ok(()),
        );

        // should fail, insufficient funds
        process_instruction(
            features,
            &serialize(&VoteInstruction::Withdraw(lamports + 1)).unwrap(),
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Err(InstructionError::InsufficientFunds),
        );

        // should pass, partial withdraw
        let withdraw_lamports = 42;
        let accounts = process_instruction(
            features,
            &serialize(&VoteInstruction::Withdraw(withdraw_lamports)).unwrap(),
            transaction_accounts,
            instruction_accounts,
            Ok(()),
        );
        assert_eq!(accounts[0].lamports(), lamports - withdraw_lamports);
        assert_eq!(accounts[3].lamports(), withdraw_lamports);
    }

    #[test_case(false ; "VoteStateV3")]
    #[test_case(true ; "VoteStateV4")]
    fn test_vote_state_withdraw(vote_state_v4: bool) {
        let authorized_withdrawer_pubkey = solana_pubkey::new_rand();
        let (vote_pubkey_1, vote_account_with_epoch_credits_1) =
            create_test_account_with_epoch_credits(vote_state_v4, &[2, 1]);
        let (vote_pubkey_2, vote_account_with_epoch_credits_2) =
            create_test_account_with_epoch_credits(vote_state_v4, &[2, 1, 3]);
        let clock = Clock {
            epoch: 3,
            ..Clock::default()
        };
        let clock_account = account::create_account_shared_data_for_test(&clock);
        let rent_sysvar = Rent::default();
        let minimum_balance = rent_sysvar
            .minimum_balance(vote_account_with_epoch_credits_1.data().len())
            .max(1);
        let lamports = vote_account_with_epoch_credits_1.lamports();
        let transaction_accounts = vec![
            (vote_pubkey_1, vote_account_with_epoch_credits_1),
            (vote_pubkey_2, vote_account_with_epoch_credits_2),
            (sysvar::clock::id(), clock_account),
            (
                sysvar::rent::id(),
                account::create_account_shared_data_for_test(&rent_sysvar),
            ),
            (authorized_withdrawer_pubkey, AccountSharedData::default()),
        ];
        let mut instruction_accounts = vec![
            AccountMeta {
                pubkey: vote_pubkey_1,
                is_signer: true,
                is_writable: true,
            },
            AccountMeta {
                pubkey: authorized_withdrawer_pubkey,
                is_signer: false,
                is_writable: true,
            },
        ];

        let features = VoteProgramFeatures {
            vote_state_v4,
            ..Default::default()
        };

        // non rent exempt withdraw, with 0 credit epoch
        instruction_accounts[0].pubkey = vote_pubkey_1;
        process_instruction(
            features,
            &serialize(&VoteInstruction::Withdraw(lamports - minimum_balance + 1)).unwrap(),
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Err(InstructionError::InsufficientFunds),
        );

        // non rent exempt withdraw, without 0 credit epoch
        instruction_accounts[0].pubkey = vote_pubkey_2;
        process_instruction(
            features,
            &serialize(&VoteInstruction::Withdraw(lamports - minimum_balance + 1)).unwrap(),
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Err(InstructionError::InsufficientFunds),
        );

        // full withdraw, with 0 credit epoch
        instruction_accounts[0].pubkey = vote_pubkey_1;
        process_instruction(
            features,
            &serialize(&VoteInstruction::Withdraw(lamports)).unwrap(),
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Ok(()),
        );

        // full withdraw, without 0 credit epoch
        instruction_accounts[0].pubkey = vote_pubkey_2;
        process_instruction(
            features,
            &serialize(&VoteInstruction::Withdraw(lamports)).unwrap(),
            transaction_accounts,
            instruction_accounts,
            Err(VoteError::ActiveVoteAccountClose.into()),
        );
    }

    fn perform_authorize_with_seed_test(
        vote_state_v4: bool,
        bls_pubkey_management_in_vote_account: bool,
        authorization_type: VoteAuthorize,
        vote_pubkey: Pubkey,
        vote_account: AccountSharedData,
        current_authority_base_key: Pubkey,
        current_authority_seed: String,
        current_authority_owner: Pubkey,
        new_authority_pubkey: Pubkey,
    ) {
        let clock = Clock {
            epoch: 1,
            leader_schedule_epoch: 2,
            ..Clock::default()
        };
        let clock_account = account::create_account_shared_data_for_test(&clock);
        let transaction_accounts = vec![
            (vote_pubkey, vote_account),
            (sysvar::clock::id(), clock_account),
            (current_authority_base_key, AccountSharedData::default()),
        ];
        let mut instruction_accounts = vec![
            AccountMeta {
                pubkey: vote_pubkey,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: sysvar::clock::id(),
                is_signer: false,
                is_writable: false,
            },
            AccountMeta {
                pubkey: current_authority_base_key,
                is_signer: true,
                is_writable: false,
            },
        ];

        let features = VoteProgramFeatures {
            vote_state_v4,
            bls_pubkey_management_in_vote_account,
            ..Default::default()
        };
        let expected_cus = if matches!(authorization_type, VoteAuthorize::VoterWithBLS(_)) {
            DEFAULT_COMPUTE_UNITS + BLS_PROOF_OF_POSSESSION_VERIFICATION_COMPUTE_UNITS
        } else {
            DEFAULT_COMPUTE_UNITS
        };

        // Can't change authority unless base key signs.
        instruction_accounts[2].is_signer = false;
        process_instruction_with_cu_check(
            features,
            &serialize(&VoteInstruction::AuthorizeWithSeed(
                VoteAuthorizeWithSeedArgs {
                    authorization_type,
                    current_authority_derived_key_owner: current_authority_owner,
                    current_authority_derived_key_seed: current_authority_seed.clone(),
                    new_authority: new_authority_pubkey,
                },
            ))
            .unwrap(),
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Err(InstructionError::MissingRequiredSignature),
            expected_cus,
        );
        instruction_accounts[2].is_signer = true;

        // Can't change authority if seed doesn't match.
        process_instruction_with_cu_check(
            features,
            &serialize(&VoteInstruction::AuthorizeWithSeed(
                VoteAuthorizeWithSeedArgs {
                    authorization_type,
                    current_authority_derived_key_owner: current_authority_owner,
                    current_authority_derived_key_seed: String::from("WRONG_SEED"),
                    new_authority: new_authority_pubkey,
                },
            ))
            .unwrap(),
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Err(InstructionError::MissingRequiredSignature),
            expected_cus,
        );

        // Can't change authority if owner doesn't match.
        process_instruction_with_cu_check(
            features,
            &serialize(&VoteInstruction::AuthorizeWithSeed(
                VoteAuthorizeWithSeedArgs {
                    authorization_type,
                    current_authority_derived_key_owner: Pubkey::new_unique(), // Wrong owner.
                    current_authority_derived_key_seed: current_authority_seed.clone(),
                    new_authority: new_authority_pubkey,
                },
            ))
            .unwrap(),
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Err(InstructionError::MissingRequiredSignature),
            expected_cus,
        );

        // Can change authority when base key signs for related derived key.
        process_instruction_with_cu_check(
            features,
            &serialize(&VoteInstruction::AuthorizeWithSeed(
                VoteAuthorizeWithSeedArgs {
                    authorization_type,
                    current_authority_derived_key_owner: current_authority_owner,
                    current_authority_derived_key_seed: current_authority_seed,
                    new_authority: new_authority_pubkey,
                },
            ))
            .unwrap(),
            transaction_accounts,
            instruction_accounts,
            Ok(()),
            expected_cus,
        );
    }

    fn perform_authorize_checked_with_seed_test(
        vote_state_v4: bool,
        bls_pubkey_management_in_vote_account: bool,
        authorization_type: VoteAuthorize,
        vote_pubkey: Pubkey,
        vote_account: AccountSharedData,
        current_authority_base_key: Pubkey,
        current_authority_seed: String,
        current_authority_owner: Pubkey,
        new_authority_pubkey: Pubkey,
    ) {
        let clock = Clock {
            epoch: 1,
            leader_schedule_epoch: 2,
            ..Clock::default()
        };
        let clock_account = account::create_account_shared_data_for_test(&clock);
        let transaction_accounts = vec![
            (vote_pubkey, vote_account),
            (sysvar::clock::id(), clock_account),
            (current_authority_base_key, AccountSharedData::default()),
            (new_authority_pubkey, AccountSharedData::default()),
        ];
        let mut instruction_accounts = vec![
            AccountMeta {
                pubkey: vote_pubkey,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: sysvar::clock::id(),
                is_signer: false,
                is_writable: false,
            },
            AccountMeta {
                pubkey: current_authority_base_key,
                is_signer: true,
                is_writable: false,
            },
            AccountMeta {
                pubkey: new_authority_pubkey,
                is_signer: true,
                is_writable: false,
            },
        ];

        let features = VoteProgramFeatures {
            vote_state_v4,
            bls_pubkey_management_in_vote_account,
            ..Default::default()
        };
        let expected_cus = if matches!(authorization_type, VoteAuthorize::VoterWithBLS(_)) {
            DEFAULT_COMPUTE_UNITS + BLS_PROOF_OF_POSSESSION_VERIFICATION_COMPUTE_UNITS
        } else {
            DEFAULT_COMPUTE_UNITS
        };

        // Can't change authority unless base key signs.
        instruction_accounts[2].is_signer = false;
        process_instruction_with_cu_check(
            features,
            &serialize(&VoteInstruction::AuthorizeCheckedWithSeed(
                VoteAuthorizeCheckedWithSeedArgs {
                    authorization_type,
                    current_authority_derived_key_owner: current_authority_owner,
                    current_authority_derived_key_seed: current_authority_seed.clone(),
                },
            ))
            .unwrap(),
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Err(InstructionError::MissingRequiredSignature),
            expected_cus,
        );
        instruction_accounts[2].is_signer = true;

        // Can't change authority unless new authority signs.
        // This check happens before authorize(), so no BLS CUs are consumed.
        instruction_accounts[3].is_signer = false;
        process_instruction(
            features,
            &serialize(&VoteInstruction::AuthorizeCheckedWithSeed(
                VoteAuthorizeCheckedWithSeedArgs {
                    authorization_type,
                    current_authority_derived_key_owner: current_authority_owner,
                    current_authority_derived_key_seed: current_authority_seed.clone(),
                },
            ))
            .unwrap(),
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Err(InstructionError::MissingRequiredSignature),
        );
        instruction_accounts[3].is_signer = true;

        // Can't change authority if seed doesn't match.
        process_instruction_with_cu_check(
            features,
            &serialize(&VoteInstruction::AuthorizeCheckedWithSeed(
                VoteAuthorizeCheckedWithSeedArgs {
                    authorization_type,
                    current_authority_derived_key_owner: current_authority_owner,
                    current_authority_derived_key_seed: String::from("WRONG_SEED"),
                },
            ))
            .unwrap(),
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Err(InstructionError::MissingRequiredSignature),
            expected_cus,
        );

        // Can't change authority if owner doesn't match.
        process_instruction_with_cu_check(
            features,
            &serialize(&VoteInstruction::AuthorizeCheckedWithSeed(
                VoteAuthorizeCheckedWithSeedArgs {
                    authorization_type,
                    current_authority_derived_key_owner: Pubkey::new_unique(), // Wrong owner.
                    current_authority_derived_key_seed: current_authority_seed.clone(),
                },
            ))
            .unwrap(),
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Err(InstructionError::MissingRequiredSignature),
            expected_cus,
        );

        // Can change authority when base key signs for related derived key and new authority signs.
        process_instruction_with_cu_check(
            features,
            &serialize(&VoteInstruction::AuthorizeCheckedWithSeed(
                VoteAuthorizeCheckedWithSeedArgs {
                    authorization_type,
                    current_authority_derived_key_owner: current_authority_owner,
                    current_authority_derived_key_seed: current_authority_seed,
                },
            ))
            .unwrap(),
            transaction_accounts,
            instruction_accounts,
            Ok(()),
            expected_cus,
        );
    }

    #[test_matrix([false, true], [false, true])]
    fn test_voter_base_key_can_authorize_new_voter(
        vote_state_v4: bool,
        bls_pubkey_management_in_vote_account: bool,
    ) {
        let VoteAccountTestFixtureWithAuthorities {
            vote_pubkey,
            voter_base_key,
            voter_owner,
            voter_seed,
            vote_account,
            ..
        } = create_test_account_with_authorized_from_seed(vote_state_v4);
        let new_voter_pubkey = Pubkey::new_unique();
        let (bls_pubkey, bls_proof_of_possession) =
            create_bls_pubkey_and_proof_of_possession(&vote_pubkey);
        let authorize_type = if vote_state_v4 && bls_pubkey_management_in_vote_account {
            VoteAuthorize::VoterWithBLS(VoterWithBLSArgs {
                bls_pubkey,
                bls_proof_of_possession,
            })
        } else {
            VoteAuthorize::Voter
        };
        perform_authorize_with_seed_test(
            vote_state_v4,
            bls_pubkey_management_in_vote_account,
            authorize_type,
            vote_pubkey,
            vote_account,
            voter_base_key,
            voter_seed,
            voter_owner,
            new_voter_pubkey,
        );
    }

    #[test_matrix([false, true], [false, true])]
    fn test_withdrawer_base_key_can_authorize_new_voter(
        vote_state_v4: bool,
        bls_pubkey_management_in_vote_account: bool,
    ) {
        let VoteAccountTestFixtureWithAuthorities {
            vote_pubkey,
            withdrawer_base_key,
            withdrawer_owner,
            withdrawer_seed,
            vote_account,
            ..
        } = create_test_account_with_authorized_from_seed(vote_state_v4);
        let new_voter_pubkey = Pubkey::new_unique();
        let (bls_pubkey, bls_proof_of_possession) =
            create_bls_pubkey_and_proof_of_possession(&vote_pubkey);
        let authorize_type = if vote_state_v4 && bls_pubkey_management_in_vote_account {
            VoteAuthorize::VoterWithBLS(VoterWithBLSArgs {
                bls_pubkey,
                bls_proof_of_possession,
            })
        } else {
            VoteAuthorize::Voter
        };
        perform_authorize_with_seed_test(
            vote_state_v4,
            bls_pubkey_management_in_vote_account,
            authorize_type,
            vote_pubkey,
            vote_account,
            withdrawer_base_key,
            withdrawer_seed,
            withdrawer_owner,
            new_voter_pubkey,
        );
    }

    #[test_case(false ; "VoteStateV3")]
    #[test_case(true ; "VoteStateV4")]
    fn test_voter_base_key_can_not_authorize_new_withdrawer(vote_state_v4: bool) {
        let VoteAccountTestFixtureWithAuthorities {
            vote_pubkey,
            voter_base_key,
            voter_owner,
            voter_seed,
            vote_account,
            ..
        } = create_test_account_with_authorized_from_seed(vote_state_v4);
        let new_withdrawer_pubkey = Pubkey::new_unique();
        let clock = Clock {
            epoch: 1,
            leader_schedule_epoch: 2,
            ..Clock::default()
        };
        let clock_account = account::create_account_shared_data_for_test(&clock);
        let transaction_accounts = vec![
            (vote_pubkey, vote_account),
            (sysvar::clock::id(), clock_account),
            (voter_base_key, AccountSharedData::default()),
        ];
        let instruction_accounts = vec![
            AccountMeta {
                pubkey: vote_pubkey,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: sysvar::clock::id(),
                is_signer: false,
                is_writable: false,
            },
            AccountMeta {
                pubkey: voter_base_key,
                is_signer: true,
                is_writable: false,
            },
        ];
        // Despite having Voter authority, you may not change the Withdrawer authority.
        process_instruction(
            VoteProgramFeatures {
                vote_state_v4,
                ..Default::default()
            },
            &serialize(&VoteInstruction::AuthorizeWithSeed(
                VoteAuthorizeWithSeedArgs {
                    authorization_type: VoteAuthorize::Withdrawer,
                    current_authority_derived_key_owner: voter_owner,
                    current_authority_derived_key_seed: voter_seed,
                    new_authority: new_withdrawer_pubkey,
                },
            ))
            .unwrap(),
            transaction_accounts,
            instruction_accounts,
            Err(InstructionError::MissingRequiredSignature),
        );
    }

    #[test_matrix([false, true], [false, true])]
    fn test_withdrawer_base_key_can_authorize_new_withdrawer(
        vote_state_v4: bool,
        bls_pubkey_management_in_vote_account: bool,
    ) {
        let VoteAccountTestFixtureWithAuthorities {
            vote_pubkey,
            withdrawer_base_key,
            withdrawer_owner,
            withdrawer_seed,
            vote_account,
            ..
        } = create_test_account_with_authorized_from_seed(vote_state_v4);
        let new_withdrawer_pubkey = Pubkey::new_unique();
        perform_authorize_with_seed_test(
            vote_state_v4,
            bls_pubkey_management_in_vote_account,
            VoteAuthorize::Withdrawer,
            vote_pubkey,
            vote_account,
            withdrawer_base_key,
            withdrawer_seed,
            withdrawer_owner,
            new_withdrawer_pubkey,
        );
    }

    #[test_matrix([false, true], [false, true])]
    fn test_voter_base_key_can_authorize_new_voter_checked(
        vote_state_v4: bool,
        bls_pubkey_management_in_vote_account: bool,
    ) {
        let VoteAccountTestFixtureWithAuthorities {
            vote_pubkey,
            voter_base_key,
            voter_owner,
            voter_seed,
            vote_account,
            ..
        } = create_test_account_with_authorized_from_seed(vote_state_v4);
        let new_voter_pubkey = Pubkey::new_unique();
        let (bls_pubkey, bls_proof_of_possession) =
            create_bls_pubkey_and_proof_of_possession(&vote_pubkey);
        let authorize_type = if vote_state_v4 && bls_pubkey_management_in_vote_account {
            VoteAuthorize::VoterWithBLS(VoterWithBLSArgs {
                bls_pubkey,
                bls_proof_of_possession,
            })
        } else {
            VoteAuthorize::Voter
        };
        perform_authorize_checked_with_seed_test(
            vote_state_v4,
            bls_pubkey_management_in_vote_account,
            authorize_type,
            vote_pubkey,
            vote_account,
            voter_base_key,
            voter_seed,
            voter_owner,
            new_voter_pubkey,
        );
    }

    #[test_matrix([false, true], [false, true])]
    fn test_withdrawer_base_key_can_authorize_new_voter_checked(
        vote_state_v4: bool,
        bls_pubkey_management_in_vote_account: bool,
    ) {
        let VoteAccountTestFixtureWithAuthorities {
            vote_pubkey,
            withdrawer_base_key,
            withdrawer_owner,
            withdrawer_seed,
            vote_account,
            ..
        } = create_test_account_with_authorized_from_seed(vote_state_v4);
        let new_voter_pubkey = Pubkey::new_unique();
        let (bls_pubkey, bls_proof_of_possession) =
            create_bls_pubkey_and_proof_of_possession(&vote_pubkey);
        let authorize_type = if vote_state_v4 && bls_pubkey_management_in_vote_account {
            VoteAuthorize::VoterWithBLS(VoterWithBLSArgs {
                bls_pubkey,
                bls_proof_of_possession,
            })
        } else {
            VoteAuthorize::Voter
        };
        perform_authorize_checked_with_seed_test(
            vote_state_v4,
            bls_pubkey_management_in_vote_account,
            authorize_type,
            vote_pubkey,
            vote_account,
            withdrawer_base_key,
            withdrawer_seed,
            withdrawer_owner,
            new_voter_pubkey,
        );
    }

    #[test_case(false ; "VoteStateV3")]
    #[test_case(true ; "VoteStateV4")]
    fn test_voter_base_key_can_not_authorize_new_withdrawer_checked(vote_state_v4: bool) {
        let VoteAccountTestFixtureWithAuthorities {
            vote_pubkey,
            voter_base_key,
            voter_owner,
            voter_seed,
            vote_account,
            ..
        } = create_test_account_with_authorized_from_seed(vote_state_v4);
        let new_withdrawer_pubkey = Pubkey::new_unique();
        let clock = Clock {
            epoch: 1,
            leader_schedule_epoch: 2,
            ..Clock::default()
        };
        let clock_account = account::create_account_shared_data_for_test(&clock);
        let transaction_accounts = vec![
            (vote_pubkey, vote_account),
            (sysvar::clock::id(), clock_account),
            (voter_base_key, AccountSharedData::default()),
            (new_withdrawer_pubkey, AccountSharedData::default()),
        ];
        let instruction_accounts = vec![
            AccountMeta {
                pubkey: vote_pubkey,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: sysvar::clock::id(),
                is_signer: false,
                is_writable: false,
            },
            AccountMeta {
                pubkey: voter_base_key,
                is_signer: true,
                is_writable: false,
            },
            AccountMeta {
                pubkey: new_withdrawer_pubkey,
                is_signer: true,
                is_writable: false,
            },
        ];
        // Despite having Voter authority, you may not change the Withdrawer authority.
        process_instruction(
            VoteProgramFeatures {
                vote_state_v4,
                ..Default::default()
            },
            &serialize(&VoteInstruction::AuthorizeCheckedWithSeed(
                VoteAuthorizeCheckedWithSeedArgs {
                    authorization_type: VoteAuthorize::Withdrawer,
                    current_authority_derived_key_owner: voter_owner,
                    current_authority_derived_key_seed: voter_seed,
                },
            ))
            .unwrap(),
            transaction_accounts,
            instruction_accounts,
            Err(InstructionError::MissingRequiredSignature),
        );
    }

    #[test_case(false ; "VoteStateV3")]
    #[test_case(true ; "VoteStateV4")]
    fn test_withdrawer_base_key_can_authorize_new_withdrawer_checked(vote_state_v4: bool) {
        let VoteAccountTestFixtureWithAuthorities {
            vote_pubkey,
            withdrawer_base_key,
            withdrawer_owner,
            withdrawer_seed,
            vote_account,
            ..
        } = create_test_account_with_authorized_from_seed(vote_state_v4);
        let new_withdrawer_pubkey = Pubkey::new_unique();
        perform_authorize_checked_with_seed_test(
            vote_state_v4,
            false,
            VoteAuthorize::Withdrawer,
            vote_pubkey,
            vote_account,
            withdrawer_base_key,
            withdrawer_seed,
            withdrawer_owner,
            new_withdrawer_pubkey,
        );
    }

    #[test_case(false ; "VoteStateV3")]
    #[test_case(true ; "VoteStateV4")]
    fn test_spoofed_vote(vote_state_v4: bool) {
        let features = VoteProgramFeatures {
            vote_state_v4,
            ..Default::default()
        };
        process_instruction_as_one_arg(
            features,
            &vote(
                &invalid_vote_state_pubkey(),
                &Pubkey::new_unique(),
                Vote::default(),
            ),
            Err(InstructionError::InvalidAccountOwner),
        );
        process_instruction_as_one_arg(
            features,
            &update_vote_state(
                &invalid_vote_state_pubkey(),
                &Pubkey::default(),
                VoteStateUpdate::default(),
            ),
            Err(InstructionError::InvalidAccountOwner),
        );
        process_instruction_as_one_arg(
            features,
            &compact_update_vote_state(
                &invalid_vote_state_pubkey(),
                &Pubkey::default(),
                VoteStateUpdate::default(),
            ),
            Err(InstructionError::InvalidAccountOwner),
        );
        process_instruction_as_one_arg(
            features,
            &tower_sync(
                &invalid_vote_state_pubkey(),
                &Pubkey::default(),
                TowerSync::default(),
            ),
            Err(InstructionError::InvalidAccountOwner),
        );
    }

    #[test_case(false ; "VoteStateV3")]
    #[test_case(true ; "VoteStateV4")]
    fn test_create_account_vote_state_1_14_11(vote_state_v4: bool) {
        let node_pubkey = Pubkey::new_unique();
        let vote_pubkey = Pubkey::new_unique();
        let instructions = create_account_with_config(
            &node_pubkey,
            &vote_pubkey,
            &VoteInit {
                node_pubkey,
                authorized_voter: vote_pubkey,
                authorized_withdrawer: vote_pubkey,
                commission: 0,
            },
            101,
            CreateVoteAccountConfig {
                space: vote_state::VoteState1_14_11::size_of() as u64,
                ..CreateVoteAccountConfig::default()
            },
        );
        // grab the `space` value from SystemInstruction::CreateAccount by directly indexing, for
        // expediency
        let space = usize::from_le_bytes(instructions[0].data[12..20].try_into().unwrap());
        assert_eq!(space, vote_state::VoteState1_14_11::size_of());
        let empty_vote_account = AccountSharedData::new(101, space, &id());

        let transaction_accounts = vec![
            (vote_pubkey, empty_vote_account),
            (node_pubkey, AccountSharedData::default()),
            (sysvar::clock::id(), create_default_clock_account()),
            (sysvar::rent::id(), create_default_rent_account()),
        ];

        // should fail, since VoteState1_14_11 isn't supported anymore
        process_instruction(
            VoteProgramFeatures {
                vote_state_v4,
                ..Default::default()
            },
            &instructions[1].data,
            transaction_accounts,
            instructions[1].accounts.clone(),
            Err(InstructionError::InvalidAccountData),
        );
    }

    #[test_case(false ; "VoteStateV3")]
    #[test_case(true ; "VoteStateV4")]
    fn test_create_account_vote_state_current(vote_state_v4: bool) {
        let node_pubkey = Pubkey::new_unique();
        let vote_pubkey = Pubkey::new_unique();
        let instructions = create_account_with_config(
            &node_pubkey,
            &vote_pubkey,
            &VoteInit {
                node_pubkey,
                authorized_voter: vote_pubkey,
                authorized_withdrawer: vote_pubkey,
                commission: 0,
            },
            101,
            CreateVoteAccountConfig {
                space: vote_state_size_of(vote_state_v4) as u64,
                ..CreateVoteAccountConfig::default()
            },
        );
        // grab the `space` value from SystemInstruction::CreateAccount by directly indexing, for
        // expediency
        let space = usize::from_le_bytes(instructions[0].data[12..20].try_into().unwrap());
        assert_eq!(space, vote_state_size_of(vote_state_v4));
        let empty_vote_account = AccountSharedData::new(101, space, &id());

        let transaction_accounts = vec![
            (vote_pubkey, empty_vote_account),
            (node_pubkey, AccountSharedData::default()),
            (sysvar::clock::id(), create_default_clock_account()),
            (sysvar::rent::id(), create_default_rent_account()),
        ];

        process_instruction(
            VoteProgramFeatures {
                vote_state_v4,
                ..Default::default()
            },
            &instructions[1].data,
            transaction_accounts,
            instructions[1].accounts.clone(),
            Ok(()),
        );
    }

    #[test_case(false ; "VoteStateV3")]
    #[test_case(true ; "VoteStateV4")]
    fn test_vote_process_instruction(vote_state_v4: bool) {
        agave_logger::setup();
        let instructions = create_account_with_config(
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            &VoteInit::default(),
            101,
            CreateVoteAccountConfig::default(),
        );
        let features = VoteProgramFeatures {
            vote_state_v4,
            ..Default::default()
        };
        // this case fails regardless of CreateVoteAccountConfig::space, because
        // process_instruction_as_one_arg passes a default (empty) account
        process_instruction_as_one_arg(
            features,
            &instructions[1],
            Err(InstructionError::InvalidAccountData),
        );
        process_instruction_as_one_arg(
            features,
            &vote(
                &Pubkey::new_unique(),
                &Pubkey::new_unique(),
                Vote::default(),
            ),
            Err(InstructionError::InvalidInstructionData),
        );
        process_instruction_as_one_arg(
            features,
            &vote_switch(
                &Pubkey::new_unique(),
                &Pubkey::new_unique(),
                Vote::default(),
                Hash::default(),
            ),
            Err(InstructionError::InvalidInstructionData),
        );
        process_instruction_as_one_arg(
            features,
            &authorize(
                &Pubkey::new_unique(),
                &Pubkey::new_unique(),
                &Pubkey::new_unique(),
                VoteAuthorize::Voter,
            ),
            Err(InstructionError::InvalidAccountData),
        );
        process_instruction_as_one_arg(
            features,
            &update_vote_state(
                &Pubkey::default(),
                &Pubkey::default(),
                VoteStateUpdate::default(),
            ),
            Err(InstructionError::InvalidInstructionData),
        );

        process_instruction_as_one_arg(
            features,
            &update_vote_state_switch(
                &Pubkey::default(),
                &Pubkey::default(),
                VoteStateUpdate::default(),
                Hash::default(),
            ),
            Err(InstructionError::InvalidInstructionData),
        );
        process_instruction_as_one_arg(
            features,
            &compact_update_vote_state(
                &Pubkey::default(),
                &Pubkey::default(),
                VoteStateUpdate::default(),
            ),
            Err(InstructionError::InvalidInstructionData),
        );
        process_instruction_as_one_arg(
            features,
            &compact_update_vote_state_switch(
                &Pubkey::default(),
                &Pubkey::default(),
                VoteStateUpdate::default(),
                Hash::default(),
            ),
            Err(InstructionError::InvalidInstructionData),
        );
        process_instruction_as_one_arg(
            features,
            &tower_sync(&Pubkey::default(), &Pubkey::default(), TowerSync::default()),
            Err(InstructionError::InvalidAccountData),
        );
        process_instruction_as_one_arg(
            features,
            &tower_sync_switch(
                &Pubkey::default(),
                &Pubkey::default(),
                TowerSync::default(),
                Hash::default(),
            ),
            Err(InstructionError::InvalidAccountData),
        );

        process_instruction_as_one_arg(
            features,
            &update_validator_identity(
                &Pubkey::new_unique(),
                &Pubkey::new_unique(),
                &Pubkey::new_unique(),
            ),
            Err(InstructionError::InvalidAccountData),
        );
        process_instruction_as_one_arg(
            features,
            &update_commission(&Pubkey::new_unique(), &Pubkey::new_unique(), 0),
            Err(InstructionError::InvalidAccountData),
        );

        process_instruction_as_one_arg(
            features,
            &withdraw(
                &Pubkey::new_unique(),
                &Pubkey::new_unique(),
                0,
                &Pubkey::new_unique(),
            ),
            Err(InstructionError::InvalidAccountData),
        );
    }

    #[test_matrix([false, true], [false, true])]
    fn test_vote_authorize_checked(
        vote_state_v4: bool,
        bls_pubkey_management_in_vote_account: bool,
    ) {
        let vote_pubkey = Pubkey::new_unique();
        let authorized_pubkey = Pubkey::new_unique();
        let new_authorized_pubkey = Pubkey::new_unique();

        let features = VoteProgramFeatures {
            vote_state_v4,
            bls_pubkey_management_in_vote_account,
            ..Default::default()
        };

        // Test with vanilla authorize accounts
        let (bls_pubkey, bls_proof_of_possession) =
            create_bls_pubkey_and_proof_of_possession(&vote_pubkey);
        let mut instruction = if vote_state_v4 && bls_pubkey_management_in_vote_account {
            authorize_checked(
                &vote_pubkey,
                &authorized_pubkey,
                &new_authorized_pubkey,
                VoteAuthorize::VoterWithBLS(VoterWithBLSArgs {
                    bls_pubkey,
                    bls_proof_of_possession,
                }),
            )
        } else {
            authorize_checked(
                &vote_pubkey,
                &authorized_pubkey,
                &new_authorized_pubkey,
                VoteAuthorize::Voter,
            )
        };
        instruction.accounts = instruction.accounts[0..2].to_vec();
        process_instruction_as_one_arg(
            features,
            &instruction,
            Err(InstructionError::MissingAccount),
        );

        let mut instruction = authorize_checked(
            &vote_pubkey,
            &authorized_pubkey,
            &new_authorized_pubkey,
            VoteAuthorize::Withdrawer,
        );
        instruction.accounts = instruction.accounts[0..2].to_vec();
        process_instruction_as_one_arg(
            features,
            &instruction,
            Err(InstructionError::MissingAccount),
        );

        // Test with non-signing new_authorized_pubkey
        let mut instruction = if vote_state_v4 && bls_pubkey_management_in_vote_account {
            authorize_checked(
                &vote_pubkey,
                &authorized_pubkey,
                &new_authorized_pubkey,
                VoteAuthorize::VoterWithBLS(VoterWithBLSArgs {
                    bls_pubkey,
                    bls_proof_of_possession,
                }),
            )
        } else {
            authorize_checked(
                &vote_pubkey,
                &authorized_pubkey,
                &new_authorized_pubkey,
                VoteAuthorize::Voter,
            )
        };
        instruction.accounts[3] = AccountMeta::new_readonly(new_authorized_pubkey, false);
        process_instruction_as_one_arg(
            features,
            &instruction,
            Err(InstructionError::MissingRequiredSignature),
        );

        let mut instruction = authorize_checked(
            &vote_pubkey,
            &authorized_pubkey,
            &new_authorized_pubkey,
            VoteAuthorize::Withdrawer,
        );
        instruction.accounts[3] = AccountMeta::new_readonly(new_authorized_pubkey, false);
        process_instruction_as_one_arg(
            features,
            &instruction,
            Err(InstructionError::MissingRequiredSignature),
        );

        // Test with new_authorized_pubkey signer
        let default_authorized_pubkey = Pubkey::default();
        let vote_account = create_test_account_with_provided_authorized(
            &default_authorized_pubkey,
            &default_authorized_pubkey,
            vote_state_v4,
        );
        let clock_address = sysvar::clock::id();
        let clock_account = account::create_account_shared_data_for_test(&Clock::default());
        let authorized_account = create_default_account();
        let new_authorized_account = create_default_account();
        let transaction_accounts = vec![
            (vote_pubkey, vote_account),
            (clock_address, clock_account),
            (default_authorized_pubkey, authorized_account),
            (new_authorized_pubkey, new_authorized_account),
        ];
        let instruction_accounts = vec![
            AccountMeta {
                pubkey: vote_pubkey,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: clock_address,
                is_signer: false,
                is_writable: false,
            },
            AccountMeta {
                pubkey: default_authorized_pubkey,
                is_signer: true,
                is_writable: false,
            },
            AccountMeta {
                pubkey: new_authorized_pubkey,
                is_signer: true,
                is_writable: false,
            },
        ];
        let (authorize_type, expected_cus) =
            if vote_state_v4 && bls_pubkey_management_in_vote_account {
                (
                    VoteAuthorize::VoterWithBLS(VoterWithBLSArgs {
                        bls_pubkey,
                        bls_proof_of_possession,
                    }),
                    DEFAULT_COMPUTE_UNITS + BLS_PROOF_OF_POSSESSION_VERIFICATION_COMPUTE_UNITS,
                )
            } else {
                (VoteAuthorize::Voter, DEFAULT_COMPUTE_UNITS)
            };
        process_instruction_with_cu_check(
            features,
            &serialize(&VoteInstruction::AuthorizeChecked(authorize_type)).unwrap(),
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Ok(()),
            expected_cus,
        );
        process_instruction(
            features,
            &serialize(&VoteInstruction::AuthorizeChecked(
                VoteAuthorize::Withdrawer,
            ))
            .unwrap(),
            transaction_accounts,
            instruction_accounts,
            Ok(()),
        );
    }

    // Explicitly covers uninitialized vote accounts for instructions.
    // `test_vote_signature` above covered:
    // * Vote
    // * UpdateVoteState
    // * CompactUpdateVoteState
    // * TowerSync
    #[test_case(false ; "VoteStateV3")]
    #[test_case(true ; "VoteStateV4")]
    fn test_uninitialized_vote_account(vote_state_v4: bool) {
        // Set up uninitialized vote account.
        let vote_pubkey = solana_pubkey::new_rand();
        let vote_account = AccountSharedData::new(100, vote_state_size_of(vote_state_v4), &id());

        let expected_error = if vote_state_v4 {
            InstructionError::UninitializedAccount
        } else {
            InstructionError::InvalidAccountData
        };

        let features = VoteProgramFeatures {
            vote_state_v4,
            ..Default::default()
        };

        // VoteInstruction::Authorize
        {
            let new_authorized_pubkey = solana_pubkey::new_rand();

            let instruction_data = serialize(&VoteInstruction::Authorize(
                new_authorized_pubkey,
                VoteAuthorize::Voter,
            ))
            .unwrap();

            let transaction_accounts = vec![
                (vote_pubkey, vote_account),
                (sysvar::clock::id(), create_default_clock_account()),
            ];

            let instruction_accounts = vec![
                AccountMeta {
                    pubkey: vote_pubkey,
                    is_signer: true,
                    is_writable: true,
                },
                AccountMeta {
                    pubkey: sysvar::clock::id(),
                    is_signer: false,
                    is_writable: false,
                },
            ];

            process_instruction(
                features,
                &instruction_data,
                transaction_accounts,
                instruction_accounts,
                Err(expected_error.clone()),
            );
        }

        // VoteInstruction::AuthorizeWithSeed
        {
            let vote_account =
                AccountSharedData::new(100, vote_state_size_of(vote_state_v4), &id());
            let current_authority_base_key = Pubkey::new_unique();
            let current_authority_owner = Pubkey::new_unique();
            let new_authority_pubkey = Pubkey::new_unique();

            let instruction_data = serialize(&VoteInstruction::AuthorizeWithSeed(
                VoteAuthorizeWithSeedArgs {
                    authorization_type: VoteAuthorize::Voter,
                    current_authority_derived_key_owner: current_authority_owner,
                    current_authority_derived_key_seed: String::from("SEED"),
                    new_authority: new_authority_pubkey,
                },
            ))
            .unwrap();

            let transaction_accounts = vec![
                (vote_pubkey, vote_account),
                (sysvar::clock::id(), create_default_clock_account()),
                (current_authority_base_key, AccountSharedData::default()),
            ];

            let instruction_accounts = vec![
                AccountMeta {
                    pubkey: vote_pubkey,
                    is_signer: false,
                    is_writable: true,
                },
                AccountMeta {
                    pubkey: sysvar::clock::id(),
                    is_signer: false,
                    is_writable: false,
                },
                AccountMeta {
                    pubkey: current_authority_base_key,
                    is_signer: true,
                    is_writable: false,
                },
            ];

            process_instruction(
                features,
                &instruction_data,
                transaction_accounts,
                instruction_accounts,
                Err(expected_error.clone()),
            );
        }

        // VoteInstruction::AuthorizeCheckedWithSeed
        {
            let vote_account =
                AccountSharedData::new(100, vote_state_size_of(vote_state_v4), &id());
            let current_authority_base_key = Pubkey::new_unique();
            let current_authority_owner = Pubkey::new_unique();
            let new_authority_pubkey = Pubkey::new_unique();

            let instruction_data = serialize(&VoteInstruction::AuthorizeCheckedWithSeed(
                VoteAuthorizeCheckedWithSeedArgs {
                    authorization_type: VoteAuthorize::Voter,
                    current_authority_derived_key_owner: current_authority_owner,
                    current_authority_derived_key_seed: String::from("SEED"),
                },
            ))
            .unwrap();

            let transaction_accounts = vec![
                (vote_pubkey, vote_account),
                (sysvar::clock::id(), create_default_clock_account()),
                (current_authority_base_key, AccountSharedData::default()),
                (new_authority_pubkey, AccountSharedData::default()),
            ];

            let instruction_accounts = vec![
                AccountMeta {
                    pubkey: vote_pubkey,
                    is_signer: false,
                    is_writable: true,
                },
                AccountMeta {
                    pubkey: sysvar::clock::id(),
                    is_signer: false,
                    is_writable: false,
                },
                AccountMeta {
                    pubkey: current_authority_base_key,
                    is_signer: true,
                    is_writable: false,
                },
                AccountMeta {
                    pubkey: new_authority_pubkey,
                    is_signer: true,
                    is_writable: false,
                },
            ];

            process_instruction(
                features,
                &instruction_data,
                transaction_accounts,
                instruction_accounts,
                Err(expected_error.clone()),
            );
        }

        // VoteInstruction::UpdateValidatorIdentity
        {
            let vote_account =
                AccountSharedData::new(100, vote_state_size_of(vote_state_v4), &id());
            let node_pubkey = Pubkey::new_unique();
            let authorized_withdrawer = Pubkey::new_unique();

            let instruction_data = serialize(&VoteInstruction::UpdateValidatorIdentity).unwrap();

            let transaction_accounts = vec![
                (vote_pubkey, vote_account),
                (node_pubkey, AccountSharedData::default()),
                (authorized_withdrawer, AccountSharedData::default()),
            ];

            let instruction_accounts = vec![
                AccountMeta {
                    pubkey: vote_pubkey,
                    is_signer: false,
                    is_writable: true,
                },
                AccountMeta {
                    pubkey: node_pubkey,
                    is_signer: true,
                    is_writable: false,
                },
                AccountMeta {
                    pubkey: authorized_withdrawer,
                    is_signer: true,
                    is_writable: false,
                },
            ];

            process_instruction(
                features,
                &instruction_data,
                transaction_accounts,
                instruction_accounts,
                Err(expected_error.clone()),
            );
        }

        // VoteInstruction::UpdateCommission
        {
            let vote_account =
                AccountSharedData::new(100, vote_state_size_of(vote_state_v4), &id());
            let authorized_withdrawer = Pubkey::new_unique();

            let instruction_data = serialize(&VoteInstruction::UpdateCommission(42)).unwrap();

            let transaction_accounts = vec![
                (vote_pubkey, vote_account),
                (authorized_withdrawer, AccountSharedData::default()),
                (
                    sysvar::clock::id(),
                    account::create_account_shared_data_for_test(&Clock::default()),
                ),
                (
                    sysvar::epoch_schedule::id(),
                    account::create_account_shared_data_for_test(&EpochSchedule::without_warmup()),
                ),
            ];

            let instruction_accounts = vec![
                AccountMeta {
                    pubkey: vote_pubkey,
                    is_signer: false,
                    is_writable: true,
                },
                AccountMeta {
                    pubkey: authorized_withdrawer,
                    is_signer: true,
                    is_writable: false,
                },
            ];

            process_instruction(
                features,
                &instruction_data,
                transaction_accounts,
                instruction_accounts,
                Err(expected_error.clone()),
            );
        }

        // VoteInstruction::Withdraw
        {
            let vote_account =
                AccountSharedData::new(100, vote_state_size_of(vote_state_v4), &id());
            let recipient = Pubkey::new_unique();

            let instruction_data = serialize(&VoteInstruction::Withdraw(10)).unwrap();

            let transaction_accounts = vec![
                (vote_pubkey, vote_account),
                (recipient, AccountSharedData::default()),
                (sysvar::clock::id(), create_default_clock_account()),
                (sysvar::rent::id(), create_default_rent_account()),
            ];

            let instruction_accounts = vec![
                AccountMeta {
                    pubkey: vote_pubkey,
                    is_signer: true,
                    is_writable: true,
                },
                AccountMeta {
                    pubkey: recipient,
                    is_signer: false,
                    is_writable: true,
                },
            ];

            process_instruction(
                features,
                &instruction_data,
                transaction_accounts,
                instruction_accounts,
                Err(expected_error.clone()),
            );
        }

        // VoteInstruction::AuthorizeChecked
        {
            let vote_account =
                AccountSharedData::new(100, vote_state_size_of(vote_state_v4), &id());
            let authorized_pubkey = Pubkey::new_unique();
            let new_authorized_pubkey = Pubkey::new_unique();

            let instruction_data =
                serialize(&VoteInstruction::AuthorizeChecked(VoteAuthorize::Voter)).unwrap();

            let transaction_accounts = vec![
                (vote_pubkey, vote_account),
                (sysvar::clock::id(), create_default_clock_account()),
                (authorized_pubkey, AccountSharedData::default()),
                (new_authorized_pubkey, AccountSharedData::default()),
            ];

            let instruction_accounts = vec![
                AccountMeta {
                    pubkey: vote_pubkey,
                    is_signer: false,
                    is_writable: true,
                },
                AccountMeta {
                    pubkey: sysvar::clock::id(),
                    is_signer: false,
                    is_writable: false,
                },
                AccountMeta {
                    pubkey: authorized_pubkey,
                    is_signer: true,
                    is_writable: false,
                },
                AccountMeta {
                    pubkey: new_authorized_pubkey,
                    is_signer: true,
                    is_writable: false,
                },
            ];

            process_instruction(
                features,
                &instruction_data,
                transaction_accounts,
                instruction_accounts,
                Err(expected_error),
            );
        }
    }

    // Test DepositDelegatorRewards instruction (SIMD-0123).
    #[test]
    fn test_deposit_delegator_rewards() {
        const DEPOSIT_DELEGATOR_REWARDS_COMPUTE_UNITS: u64 =
            DEFAULT_COMPUTE_UNITS + SYSTEM_PROGRAM_COMPUTE_UNITS;

        let (vote_pubkey, _authorized_voter, _authorized_withdrawer, vote_account_v4) =
            create_test_account_with_authorized(true);
        let (vote_pubkey_v3, vote_account_v3) = create_test_account(false);

        // Create source account with enough lamports to transfer.
        let source_pubkey = Pubkey::new_unique();
        let source_lamports = 1_000_000;
        let source_account =
            AccountSharedData::new(source_lamports, 0, &solana_sdk_ids::system_program::id());

        let deposit_amount = 100_000;

        let instruction_data = serialize(&VoteInstruction::DepositDelegatorRewards {
            deposit: deposit_amount,
        })
        .unwrap();

        let instruction_accounts = vec![
            AccountMeta {
                pubkey: vote_pubkey,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: source_pubkey,
                is_signer: true,
                is_writable: true,
            },
            AccountMeta {
                pubkey: solana_sdk_ids::system_program::id(),
                is_signer: false,
                is_writable: false,
            },
        ];

        let transaction_accounts = vec![
            (vote_pubkey, vote_account_v4.clone()),
            (source_pubkey, source_account.clone()),
            (
                solana_sdk_ids::system_program::id(),
                AccountSharedData::new(0, 0, &solana_sdk_ids::native_loader::id()),
            ),
        ];

        // Fail - SIMD-0291: commission_rate_in_basis_points disabled.
        process_instruction(
            VoteProgramFeatures {
                vote_state_v4: true,
                commission_rate_in_basis_points: false,
                custom_commission_collector: true,
                block_revenue_sharing: true,
                ..Default::default()
            },
            &instruction_data,
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Err(InstructionError::InvalidInstructionData),
        );

        // Fail - SIMD-0232: custom_commission_collector disabled.
        process_instruction(
            VoteProgramFeatures {
                vote_state_v4: true,
                commission_rate_in_basis_points: true,
                custom_commission_collector: false,
                block_revenue_sharing: true,
                ..Default::default()
            },
            &instruction_data,
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Err(InstructionError::InvalidInstructionData),
        );

        // Fail - SIMD-0123: block_revenue_sharing disabled.
        process_instruction(
            VoteProgramFeatures {
                vote_state_v4: true,
                commission_rate_in_basis_points: true,
                custom_commission_collector: true,
                block_revenue_sharing: false,
                ..Default::default()
            },
            &instruction_data,
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Err(InstructionError::InvalidInstructionData),
        );

        // Fail - SIMD-0185: vote_state_v4 disabled (target_version is V3).
        process_instruction(
            VoteProgramFeatures {
                vote_state_v4: false,
                commission_rate_in_basis_points: true,
                custom_commission_collector: true,
                block_revenue_sharing: true,
                ..Default::default()
            },
            &instruction_data,
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Err(InstructionError::InvalidInstructionData),
        );

        // Fail - Not enough accounts (less than 2).
        let single_account_instruction_accounts = vec![AccountMeta {
            pubkey: vote_pubkey,
            is_signer: false,
            is_writable: true,
        }];
        process_instruction(
            VoteProgramFeatures::all_enabled(),
            &instruction_data,
            transaction_accounts.clone(),
            single_account_instruction_accounts,
            Err(InstructionError::MissingAccount),
        );

        // Fail - Source account is not a signer.
        let non_signer_instruction_accounts = vec![
            AccountMeta {
                pubkey: vote_pubkey,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: source_pubkey,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: solana_sdk_ids::system_program::id(),
                is_signer: false,
                is_writable: false,
            },
        ];
        process_instruction(
            VoteProgramFeatures::all_enabled(),
            &instruction_data,
            transaction_accounts.clone(),
            non_signer_instruction_accounts,
            Err(InstructionError::MissingRequiredSignature),
        );

        // Fail - Vote account fails to deserialize (zeroed/uninitialized data).
        let invalid_vote_account = AccountSharedData::new(1_000_000, VoteStateV4::size_of(), &id());
        process_instruction(
            VoteProgramFeatures::all_enabled(),
            &instruction_data,
            vec![
                (vote_pubkey, invalid_vote_account),
                (source_pubkey, source_account.clone()),
            ],
            instruction_accounts.clone(),
            Err(InstructionError::UninitializedAccount),
        );

        // Fail - Vote account is initialized but V3 (not V4).
        let instruction_accounts_v3 = vec![
            AccountMeta {
                pubkey: vote_pubkey_v3,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: source_pubkey,
                is_signer: true,
                is_writable: true,
            },
        ];
        process_instruction(
            VoteProgramFeatures::all_enabled(),
            &instruction_data,
            vec![
                (vote_pubkey_v3, vote_account_v3),
                (source_pubkey, source_account.clone()),
            ],
            instruction_accounts_v3,
            Err(InstructionError::InvalidAccountData),
        );

        // Fail - non-system-owned source account.
        let non_system_source_account = AccountSharedData::new(1_000_000, 0, &Pubkey::new_unique());
        process_instruction_with_cu_check(
            VoteProgramFeatures::all_enabled(),
            &instruction_data,
            vec![
                (vote_pubkey, vote_account_v4.clone()),
                (source_pubkey, non_system_source_account),
                (
                    solana_sdk_ids::system_program::id(),
                    AccountSharedData::new(0, 0, &solana_sdk_ids::native_loader::id()),
                ),
            ],
            instruction_accounts.clone(),
            Err(InstructionError::ExternalAccountLamportSpend),
            DEPOSIT_DELEGATOR_REWARDS_COMPUTE_UNITS,
        );

        // Fail - source account == destination account.
        process_instruction_with_cu_check(
            VoteProgramFeatures::all_enabled(),
            &instruction_data,
            vec![
                (vote_pubkey, vote_account_v4.clone()),
                (
                    solana_sdk_ids::system_program::id(),
                    AccountSharedData::new(0, 0, &solana_sdk_ids::native_loader::id()),
                ),
            ],
            vec![
                AccountMeta {
                    pubkey: vote_pubkey,
                    is_signer: false,
                    is_writable: true,
                },
                AccountMeta {
                    pubkey: vote_pubkey, // Duplicated
                    is_signer: true,
                    is_writable: true,
                },
                AccountMeta {
                    pubkey: solana_sdk_ids::system_program::id(),
                    is_signer: false,
                    is_writable: false,
                },
            ],
            // CPI dedup resolves to index 0 (non-signer), causing escalation.
            Err(InstructionError::PrivilegeEscalation),
            DEFAULT_COMPUTE_UNITS,
        );

        // Fail - deposit overflow.
        let deposit_amount = 100_000;
        let mut vote_account_near_max = vote_account_v4.clone();
        {
            let mut vote_state =
                VoteStateV4::deserialize(vote_account_near_max.data(), &vote_pubkey).unwrap();
            vote_state.pending_delegator_rewards = u64::MAX - deposit_amount + 1;
            vote_account_near_max
                .set_data_from_slice(&VoteStateHandler::new_v4(vote_state).serialize());
        }

        let instruction_data = serialize(&VoteInstruction::DepositDelegatorRewards {
            deposit: deposit_amount,
        })
        .unwrap();

        process_instruction_with_cu_check(
            VoteProgramFeatures::all_enabled(),
            &instruction_data,
            vec![
                (vote_pubkey, vote_account_near_max),
                (source_pubkey, source_account.clone()),
                (
                    solana_sdk_ids::system_program::id(),
                    AccountSharedData::new(0, 0, &solana_sdk_ids::native_loader::id()),
                ),
            ],
            instruction_accounts.clone(),
            Err(InstructionError::ArithmeticOverflow),
            DEPOSIT_DELEGATOR_REWARDS_COMPUTE_UNITS,
        );

        // Success
        let resulting_accounts = process_instruction_with_cu_check(
            VoteProgramFeatures::all_enabled(),
            &instruction_data,
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Ok(()),
            DEPOSIT_DELEGATOR_REWARDS_COMPUTE_UNITS,
        );

        // Vote account should have been credited `deposit_amount`.
        // Source account should have been debited `deposit_amount`.
        // Vote state's `pending_delegator_rewards` should be updated.
        let vote_account_starting_lamports = vote_account_v4.lamports();
        let source_account_starting_lamports = source_lamports;
        let resulting_vote_account = &resulting_accounts[0];
        let resulting_source_account = &resulting_accounts[1];
        let vote_state =
            deserialize_vote_state_for_test(true, resulting_vote_account.data(), &vote_pubkey);
        assert_eq!(
            resulting_vote_account.lamports(),
            vote_account_starting_lamports + deposit_amount,
        );
        assert_eq!(
            resulting_source_account.lamports(),
            source_account_starting_lamports - deposit_amount,
        );
        assert_eq!(
            vote_state.as_ref_v4().pending_delegator_rewards,
            deposit_amount,
        );

        // Run it again with a new deposit amount.
        let first_deposit_amount = deposit_amount;
        let second_deposit_amount = 250_000;
        let vote_account_starting_lamports = resulting_vote_account.lamports();
        let source_account_starting_lamports = resulting_source_account.lamports();

        let instruction_data = serialize(&VoteInstruction::DepositDelegatorRewards {
            deposit: second_deposit_amount,
        })
        .unwrap();

        let resulting_accounts = process_instruction_with_cu_check(
            VoteProgramFeatures::all_enabled(),
            &instruction_data,
            vec![
                (vote_pubkey, resulting_vote_account.clone()),
                (source_pubkey, resulting_source_account.clone()),
                (
                    solana_sdk_ids::system_program::id(),
                    AccountSharedData::new(0, 0, &solana_sdk_ids::native_loader::id()),
                ),
            ],
            instruction_accounts.clone(),
            Ok(()),
            DEPOSIT_DELEGATOR_REWARDS_COMPUTE_UNITS,
        );

        let resulting_vote_account = &resulting_accounts[0];
        let resulting_source_account = &resulting_accounts[1];
        let vote_state =
            deserialize_vote_state_for_test(true, resulting_vote_account.data(), &vote_pubkey);
        assert_eq!(
            resulting_vote_account.lamports(),
            vote_account_starting_lamports + second_deposit_amount,
        );
        assert_eq!(
            resulting_source_account.lamports(),
            source_account_starting_lamports - second_deposit_amount,
        );
        assert_eq!(
            vote_state.as_ref_v4().pending_delegator_rewards,
            first_deposit_amount + second_deposit_amount,
        );

        // Success - zero-lamport deposit.
        let vote_account_starting_lamports = vote_account_v4.lamports();
        let source_account_starting_lamports = source_lamports;
        let instruction_data =
            serialize(&VoteInstruction::DepositDelegatorRewards { deposit: 0 }).unwrap();

        let resulting_accounts = process_instruction_with_cu_check(
            VoteProgramFeatures::all_enabled(),
            &instruction_data,
            transaction_accounts,
            instruction_accounts.clone(),
            Ok(()),
            DEPOSIT_DELEGATOR_REWARDS_COMPUTE_UNITS,
        );

        let resulting_vote_account = &resulting_accounts[0];
        let resulting_source_account = &resulting_accounts[1];
        let vote_state =
            deserialize_vote_state_for_test(true, resulting_vote_account.data(), &vote_pubkey);
        assert_eq!(
            resulting_vote_account.lamports(),
            vote_account_starting_lamports, // No-op
        );
        assert_eq!(
            resulting_source_account.lamports(),
            source_account_starting_lamports, // No-op
        );
        assert_eq!(
            vote_state.as_ref_v4().pending_delegator_rewards,
            0, // No-op
        );
    }

    #[test]
    #[allow(clippy::arithmetic_side_effects)]
    fn test_withdraw_pending_delegator_rewards() {
        let rent_sysvar = Rent::default();
        let rent_minimum_balance = rent_sysvar.minimum_balance(VoteStateV4::size_of());

        let pending_rewards = 500_000;
        let extra_for_withdraw = 100_000;
        let vote_account_lamports = rent_minimum_balance + pending_rewards + extra_for_withdraw;

        let (vote_pubkey, _authorized_voter, authorized_withdrawer, mut vote_account) =
            create_test_account_with_authorized(true);

        // Set some pending delegator rewards.
        {
            let mut vote_state =
                VoteStateV4::deserialize(vote_account.data(), &vote_pubkey).unwrap();
            vote_state.pending_delegator_rewards = pending_rewards;
            vote_account.set_data_from_slice(&VoteStateHandler::new_v4(vote_state).serialize());
            vote_account.set_lamports(vote_account_lamports);
        };

        let features = VoteProgramFeatures::all_enabled();

        let instruction_accounts = vec![
            AccountMeta {
                pubkey: vote_pubkey,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: authorized_withdrawer,
                is_signer: true,
                is_writable: true,
            },
        ];

        let rent_account = account::create_account_shared_data_for_test(&rent_sysvar);
        let transaction_accounts = vec![
            (vote_pubkey, vote_account.clone()),
            (authorized_withdrawer, AccountSharedData::default()),
            (sysvar::clock::id(), create_default_clock_account()),
            (sysvar::rent::id(), rent_account.clone()),
        ];

        // Should fail, can't close vote account when
        // pending_delegator_rewards > 0.
        process_instruction(
            features,
            &serialize(&VoteInstruction::Withdraw(vote_account_lamports)).unwrap(),
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Err(InstructionError::InsufficientFunds),
        );

        // Should fail, can't withdraw more than
        // (lamports - pending_delegator_rewards - rent_exempt).
        process_instruction(
            features,
            &serialize(&VoteInstruction::Withdraw(vote_account_lamports + 1)).unwrap(),
            transaction_accounts.clone(),
            instruction_accounts.clone(),
            Err(InstructionError::InsufficientFunds),
        );

        // Should pass, can withdraw up to the max withdrawable amount.
        for i in 1..10 {
            let withdraw_amount = 1 + i * extra_for_withdraw / 10;

            let accounts = process_instruction(
                features,
                &serialize(&VoteInstruction::Withdraw(withdraw_amount)).unwrap(),
                transaction_accounts.clone(),
                instruction_accounts.clone(),
                Ok(()),
            );

            assert_eq!(
                accounts[0].lamports(),
                vote_account_lamports - withdraw_amount
            );
            assert!(accounts[0].lamports() >= rent_minimum_balance + pending_rewards);
            assert_eq!(accounts[1].lamports(), withdraw_amount);
        }

        // Now clear pending delegator rewards.
        {
            let mut vote_state =
                VoteStateV4::deserialize(vote_account.data(), &vote_pubkey).unwrap();
            vote_state.pending_delegator_rewards = 0;
            vote_account.set_data_from_slice(&VoteStateHandler::new_v4(vote_state).serialize());
            vote_account.set_lamports(vote_account_lamports);
        };

        // Should pass, no pending delegator rewards, so we can close the whole
        // thing out.
        let accounts = process_instruction(
            features,
            &serialize(&VoteInstruction::Withdraw(vote_account_lamports)).unwrap(),
            vec![
                (vote_pubkey, vote_account.clone()),
                (authorized_withdrawer, AccountSharedData::default()),
                (sysvar::clock::id(), create_default_clock_account()),
                (sysvar::rent::id(), rent_account),
            ],
            instruction_accounts.clone(),
            Ok(()),
        );

        assert_eq!(accounts[0].lamports(), 0);
        assert_eq!(accounts[0].data(), vec![0; VoteStateV4::size_of()]);
        assert_eq!(accounts[1].lamports(), vote_account_lamports);
    }
}
