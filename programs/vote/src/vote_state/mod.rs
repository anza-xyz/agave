//! Vote state, vote program
//! Receive and processes votes from validators

pub mod handler;

pub use solana_vote_interface::state::{vote_state_versions::*, *};
use {
    handler::{VoteStateHandler, VoteStateTargetVersion},
    solana_account::{AccountSharedData, WritableAccount},
    solana_bls_signatures::{VerifiableProofOfPossession, keypair::Keypair as BLSKeypair},
    solana_clock::{Clock, Slot},
    solana_epoch_schedule::EpochSchedule,
    solana_instruction_error::InstructionError,
    solana_program_runtime::invoke_context::InvokeContext,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_sdk_ids::system_program,
    solana_system_interface::instruction as system_instruction,
    solana_transaction_context::{
        IndexOfAccount, instruction::InstructionContext,
        instruction_accounts::BorrowedInstructionAccount,
    },
    solana_vote_interface::{error::VoteError, instruction::CommissionKind, program::id},
    std::collections::HashSet,
};

fn get_vote_state_handler_checked(
    vote_account: &BorrowedInstructionAccount,
    target_version: VoteStateTargetVersion,
) -> Result<VoteStateHandler, InstructionError> {
    match target_version {
        VoteStateTargetVersion::V4 => {
            // New flow after v4 feature gate activation:
            // 1. Deserialize as `VoteStateVersions`
            // 2. Check for uninitialized
            // 3. Convert
            let versioned = VoteStateVersions::deserialize(vote_account.get_data())?;
            if versioned.is_uninitialized() {
                return Err(InstructionError::UninitializedAccount);
            }
            let vote_state =
                handler::try_convert_to_vote_state_v4(versioned, vote_account.get_key())?;
            Ok(VoteStateHandler::new_v4(vote_state))
        }
    }
}

/// Authorize the given pubkey to withdraw or sign votes. This may be called multiple times,
/// but will implicitly withdraw authorization from the previously authorized
/// key
pub fn authorize<S: std::hash::BuildHasher, F>(
    vote_account: &mut BorrowedInstructionAccount,
    target_version: VoteStateTargetVersion,
    authorized: &Pubkey,
    vote_authorize: VoteAuthorize,
    signers: &HashSet<Pubkey, S>,
    clock: &Clock,
    is_vote_authorize_with_bls_enabled: bool,
    consume_pop_compute_units: F,
) -> Result<(), InstructionError>
where
    F: FnOnce() -> Result<(), InstructionError>,
{
    let mut vote_state = get_vote_state_handler_checked(vote_account, target_version)?;

    match vote_authorize {
        VoteAuthorize::Voter => {
            if is_vote_authorize_with_bls_enabled && vote_state.has_bls_pubkey() {
                return Err(InstructionError::InvalidInstructionData);
            }
            let authorized_withdrawer_signer =
                verify_authorized_signer(vote_state.authorized_withdrawer(), signers).is_ok();

            vote_state.set_new_authorized_voter(
                authorized,
                clock.epoch,
                clock
                    .leader_schedule_epoch
                    .checked_add(1)
                    .ok_or(InstructionError::InvalidAccountData)?,
                None,
                |epoch_authorized_voter| {
                    // current authorized withdrawer or authorized voter must say "yay"
                    if authorized_withdrawer_signer {
                        Ok(())
                    } else {
                        verify_authorized_signer(&epoch_authorized_voter, signers)
                    }
                },
            )?;
        }
        VoteAuthorize::Withdrawer => {
            // current authorized withdrawer must say "yay"
            verify_authorized_signer(vote_state.authorized_withdrawer(), signers)?;
            vote_state.set_authorized_withdrawer(*authorized);
        }
        VoteAuthorize::VoterWithBLS(args) => {
            if !is_vote_authorize_with_bls_enabled {
                return Err(InstructionError::InvalidInstructionData);
            }
            let authorized_withdrawer_signer =
                verify_authorized_signer(vote_state.authorized_withdrawer(), signers).is_ok();

            verify_bls_proof_of_possession(
                vote_account.get_key(),
                &args.bls_pubkey,
                &args.bls_proof_of_possession,
                consume_pop_compute_units,
            )?;

            vote_state.set_new_authorized_voter(
                authorized,
                clock.epoch,
                clock
                    .leader_schedule_epoch
                    .checked_add(1)
                    .ok_or(InstructionError::InvalidAccountData)?,
                Some(&args.bls_pubkey),
                |epoch_authorized_voter| {
                    // current authorized withdrawer or authorized voter must say "yay"
                    if authorized_withdrawer_signer {
                        Ok(())
                    } else {
                        verify_authorized_signer(&epoch_authorized_voter, signers)
                    }
                },
            )?;
        }
    }

    vote_state.set_vote_account_state(vote_account)
}

/// Update the node_pubkey, requires signature of the authorized voter
pub fn update_validator_identity<S: std::hash::BuildHasher>(
    vote_account: &mut BorrowedInstructionAccount,
    target_version: VoteStateTargetVersion,
    node_pubkey: &Pubkey,
    signers: &HashSet<Pubkey, S>,
    custom_commission_collector_enabled: bool,
) -> Result<(), InstructionError> {
    let mut vote_state = get_vote_state_handler_checked(vote_account, target_version)?;

    // current authorized withdrawer must say "yay"
    verify_authorized_signer(vote_state.authorized_withdrawer(), signers)?;

    // new node must say "yay"
    verify_authorized_signer(node_pubkey, signers)?;

    vote_state.set_node_pubkey(*node_pubkey);

    // Before SIMD-0232, block_revenue_collector is always synced with node_pubkey.
    // After SIMD-0232, the collector can be set independently.
    if !custom_commission_collector_enabled {
        vote_state.set_block_revenue_collector(*node_pubkey);
    }

    vote_state.set_vote_account_state(vote_account)
}

/// Update the vote account's commission
pub fn update_commission<S: std::hash::BuildHasher>(
    vote_account: &mut BorrowedInstructionAccount,
    target_version: VoteStateTargetVersion,
    commission: u8,
    signers: &HashSet<Pubkey, S>,
    epoch_schedule: &EpochSchedule,
    clock: &Clock,
    disable_commission_update_rule: bool,
) -> Result<(), InstructionError> {
    let vote_state_result = get_vote_state_handler_checked(vote_account, target_version);
    let enforce_commission_update_rule = !disable_commission_update_rule
        && match vote_state_result.as_ref() {
            Ok(decoded_vote_state) => commission > decoded_vote_state.commission(),
            Err(_) => true,
        };

    if enforce_commission_update_rule && !is_commission_update_allowed(clock.slot, epoch_schedule) {
        return Err(VoteError::CommissionUpdateTooLate.into());
    }

    let mut vote_state = vote_state_result?;

    // current authorized withdrawer must say "yay"
    verify_authorized_signer(vote_state.authorized_withdrawer(), signers)?;

    vote_state.set_commission(commission);

    vote_state.set_vote_account_state(vote_account)
}

/// Update the vote account's commission in basis points (SIMD-0291, SIMD-0123).
pub fn update_commission_bps<S: std::hash::BuildHasher>(
    vote_account: &mut BorrowedInstructionAccount,
    target_version: VoteStateTargetVersion,
    commission_bps: u16,
    kind: CommissionKind,
    signers: &HashSet<Pubkey, S>,
    block_revenue_sharing_enabled: bool,
) -> Result<(), InstructionError> {
    // Per SIMD-0291: BlockRevenue returns InvalidInstructionData unless
    // SIMD-0123 (block_revenue_sharing) is enabled.
    if matches!(kind, CommissionKind::BlockRevenue) && !block_revenue_sharing_enabled {
        return Err(InstructionError::InvalidInstructionData);
    }

    let mut vote_state = get_vote_state_handler_checked(vote_account, target_version)?;

    // No commission update rule, per SIMD-0249 and SIMD-0291.

    // Require authorized withdrawer to sign.
    verify_authorized_signer(vote_state.authorized_withdrawer(), signers)?;

    match kind {
        CommissionKind::InflationRewards => {
            vote_state.set_inflation_rewards_commission_bps(commission_bps);
        }
        CommissionKind::BlockRevenue => {
            vote_state.set_block_revenue_commission_bps(commission_bps);
        }
    }

    vote_state.set_vote_account_state(vote_account)
}

/// Validates the collector per SIMD-0232 and returns its pubkey.
///
/// The designated commission collector must either be equal to the vote
/// account's address OR satisfy ALL of the following constraints:
///
/// 1. Must be a system program owned account.
/// 2. Must be rent-exempt.
/// 3. Must not be a reserved account (checked via writable flag).
fn validate_and_resolve_collector_key(
    instruction_context: &InstructionContext,
    vote_account: &BorrowedInstructionAccount,
    collector_index: IndexOfAccount,
    rent: &Rent,
) -> Result<Pubkey, InstructionError> {
    if instruction_context.get_key_of_instruction_account(collector_index)?
        == vote_account.get_key()
    {
        return Ok(*vote_account.get_key());
    }

    let collector_account = instruction_context.try_borrow_instruction_account(collector_index)?;

    // 1. Must be a system program owned account.
    if collector_account.get_owner() != &system_program::id() {
        return Err(InstructionError::InvalidAccountOwner);
    }

    // 2. Must be rent-exempt.
    if !rent.is_exempt(
        collector_account.get_lamports(),
        collector_account.get_data().len(),
    ) {
        return Err(InstructionError::InsufficientFunds);
    }

    // 3. Must not be a reserved account (checked via writable flag).
    if !collector_account.is_writable() {
        return Err(InstructionError::InvalidArgument);
    }

    Ok(*collector_account.get_key())
}

/// Update the vote account's commission collector (SIMD-0232).
pub fn update_commission_collector<S: std::hash::BuildHasher>(
    instruction_context: &InstructionContext,
    vote_account_index: IndexOfAccount,
    target_version: VoteStateTargetVersion,
    new_collector_index: IndexOfAccount,
    kind: CommissionKind,
    signers: &HashSet<Pubkey, S>,
    rent: &Rent,
) -> Result<(), InstructionError> {
    let mut vote_account =
        instruction_context.try_borrow_instruction_account(vote_account_index)?;
    let mut vote_state = get_vote_state_handler_checked(&vote_account, target_version)?;

    // Require authorized withdrawer to sign.
    verify_authorized_signer(vote_state.authorized_withdrawer(), signers)?;

    let new_collector_key = validate_and_resolve_collector_key(
        instruction_context,
        &vote_account,
        new_collector_index,
        rent,
    )?;

    match kind {
        CommissionKind::InflationRewards => {
            vote_state.set_inflation_rewards_collector(new_collector_key);
        }
        CommissionKind::BlockRevenue => {
            vote_state.set_block_revenue_collector(new_collector_key);
        }
    }

    vote_state.set_vote_account_state(&mut vote_account)
}

/// Deposit delegator rewards into a vote account (SIMD-0123).
pub fn deposit_delegator_rewards<S: std::hash::BuildHasher>(
    invoke_context: &mut InvokeContext,
    vote_account_index: IndexOfAccount,
    sender_account_index: IndexOfAccount,
    deposit: u64,
    signers: &HashSet<Pubkey, S>,
) -> Result<(), InstructionError> {
    let transaction_context = &invoke_context.transaction_context;
    let instruction_context = transaction_context.get_current_instruction_context()?;

    let vote_address = *instruction_context.get_key_of_instruction_account(vote_account_index)?;
    let source_address =
        *instruction_context.get_key_of_instruction_account(sender_account_index)?;

    // Source account must sign the transfer.
    verify_authorized_signer(&source_address, signers)?;

    // SIMD-0123 states we must validate the vote account deserializes to a v4
    // *before* attempting CPI, then update the `pending_delegator_rewards`
    // field *last*.
    // We can deserialize it, and hold onto the deserialized payload in-memory.
    // This way, we can drop the account borrow but avoid re-deserializing
    // later, since we know only lamports will change.
    let mut vote_state = {
        let vote_account =
            instruction_context.try_borrow_instruction_account(vote_account_index)?;

        // Can't use `get_vote_state_handler_checked`, since it will convert
        // the underlying vote state to v4.
        // SIMD-0123 requires an *initialized v4*.
        let versioned = VoteStateVersions::deserialize(vote_account.get_data())?;
        if let VoteStateVersions::V4(vote_state_v4) = versioned {
            Ok(VoteStateHandler::new_v4(*vote_state_v4))
        } else {
            Err(InstructionError::InvalidAccountData)
        }
    }?;

    // CPI to System: Transfer from sender to vote account.
    invoke_context.native_invoke_signed(
        system_instruction::transfer(&source_address, &vote_address, deposit),
        &[],
    )?;

    // Update `pending_delegator_rewards`.
    let transaction_context = &invoke_context.transaction_context;
    let instruction_context = transaction_context.get_current_instruction_context()?;
    let mut vote_account =
        instruction_context.try_borrow_instruction_account(vote_account_index)?;

    vote_state.add_pending_delegator_rewards(deposit)?;
    vote_state.set_vote_account_state(&mut vote_account)
}

/// Given the current slot and epoch schedule, determine if a commission change
/// is allowed
pub fn is_commission_update_allowed(slot: Slot, epoch_schedule: &EpochSchedule) -> bool {
    // always allowed during warmup epochs
    if let Some(relative_slot) = slot
        .saturating_sub(epoch_schedule.first_normal_slot)
        .checked_rem(epoch_schedule.slots_per_epoch)
    {
        // allowed up to the midpoint of the epoch
        relative_slot.saturating_mul(2) <= epoch_schedule.slots_per_epoch
    } else {
        // no slots per epoch, just allow it, even though this should never happen
        true
    }
}

fn verify_authorized_signer<S: std::hash::BuildHasher>(
    authorized: &Pubkey,
    signers: &HashSet<Pubkey, S>,
) -> Result<(), InstructionError> {
    if signers.contains(authorized) {
        Ok(())
    } else {
        Err(InstructionError::MissingRequiredSignature)
    }
}

// The message size is fixed:
// "ALPENGLOW" (9) + Vote Pubkey (32) = 41 bytes
// Note: The BLS Pubkey (48 bytes) is appended dynamically by the
// solana-bls-signatures crate.
const POP_MESSAGE_SIZE: usize = 9 + size_of::<Pubkey>();

pub(crate) fn generate_pop_message(vote_account_pubkey: &Pubkey) -> [u8; POP_MESSAGE_SIZE] {
    const LABEL_LEN: usize = 9;
    const PUBKEY_LEN: usize = size_of::<Pubkey>();

    const LABEL_START: usize = 0;
    const LABEL_END: usize = LABEL_START + LABEL_LEN;

    const PUBKEY_START: usize = LABEL_END;
    const PUBKEY_END: usize = PUBKEY_START + PUBKEY_LEN;

    // Make sure POP_MESSAGE_SIZE matches the layout at compile time
    const _: () = assert!(PUBKEY_END == POP_MESSAGE_SIZE);

    let mut message = [0u8; POP_MESSAGE_SIZE];

    message[LABEL_START..LABEL_END].copy_from_slice(b"ALPENGLOW");
    message[PUBKEY_START..PUBKEY_END].copy_from_slice(vote_account_pubkey.as_ref());

    message
}

pub fn verify_bls_proof_of_possession<F>(
    vote_account_pubkey: &Pubkey,
    bls_pubkey_compressed_bytes: &[u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE],
    bls_proof_of_possession_compressed_bytes: &[u8; BLS_PROOF_OF_POSSESSION_COMPRESSED_SIZE],
    consume_pop_compute_units: F,
) -> Result<(), InstructionError>
where
    F: FnOnce() -> Result<(), InstructionError>,
{
    // Consume CUs for BLS verification (SIMD-0387).
    consume_pop_compute_units()?;

    let message = generate_pop_message(vote_account_pubkey);
    bls_proof_of_possession_compressed_bytes
        .verify(bls_pubkey_compressed_bytes, Some(&message))
        .map_err(|_| InstructionError::InvalidArgument)
}

/// Withdraw funds from the vote account
pub fn withdraw<S: std::hash::BuildHasher>(
    instruction_context: &InstructionContext,
    vote_account_index: IndexOfAccount,
    target_version: VoteStateTargetVersion,
    lamports: u64,
    to_account_index: IndexOfAccount,
    signers: &HashSet<Pubkey, S>,
    rent_sysvar: &Rent,
    clock: &Clock,
) -> Result<(), InstructionError> {
    let mut vote_account =
        instruction_context.try_borrow_instruction_account(vote_account_index)?;
    let vote_state = get_vote_state_handler_checked(&vote_account, target_version)?;

    verify_authorized_signer(vote_state.authorized_withdrawer(), signers)?;

    let remaining_balance = vote_account
        .get_lamports()
        .checked_sub(lamports)
        .ok_or(InstructionError::InsufficientFunds)?;

    // Always zero until SIMD-0123 is activated.
    let pending_delegator_rewards = vote_state.pending_delegator_rewards();

    if remaining_balance == 0 {
        // SIMD-0123: vote account cannot be closed if
        // pending_delegator_rewards > 0.
        if pending_delegator_rewards > 0 {
            return Err(InstructionError::InsufficientFunds);
        }

        let reject_active_vote_account_close = vote_state
            .epoch_credits()
            .last()
            .map(|(last_epoch_with_credits, _, _)| {
                let current_epoch = clock.epoch;
                // if current_epoch - last_epoch_with_credits < 2 then the validator has received credits
                // either in the current epoch or the previous epoch. If it's >= 2 then it has been at least
                // one full epoch since the validator has received credits.
                current_epoch.saturating_sub(*last_epoch_with_credits) < 2
            })
            .unwrap_or(false);

        if reject_active_vote_account_close {
            return Err(VoteError::ActiveVoteAccountClose.into());
        } else {
            // Deinitialize upon zero-balance
            VoteStateHandler::deinitialize_vote_account_state(&mut vote_account, target_version)?;
        }
    } else {
        // SIMD-0123: withdrawable balance when pending_delegator_rewards > 0
        // is lamports - pending_delegator_rewards - rent_exempt_minimum.
        let min_rent_exempt_balance = rent_sysvar.minimum_balance(vote_account.get_data().len());
        let min_balance = min_rent_exempt_balance
            .checked_add(pending_delegator_rewards)
            .ok_or(InstructionError::ArithmeticOverflow)?;
        if remaining_balance < min_balance {
            return Err(InstructionError::InsufficientFunds);
        }
    }

    vote_account.checked_sub_lamports(lamports)?;
    drop(vote_account);
    let mut to_account = instruction_context.try_borrow_instruction_account(to_account_index)?;
    to_account.checked_add_lamports(lamports)?;
    Ok(())
}

/// Initialize the vote_state for a vote account using VoteInitV2
/// Assumes that the account is being init as part of a account creation or
/// balance transfer and that the transaction must be signed by the staker's
/// keys.
///
/// Also validates the inflation-rewards and block-revenue collector accounts
/// per SIMD-0464 (which delegates to the SIMD-0232 collector checks) and
/// verifies the BLS proof of possession for the authorized voter BLS pubkey.
#[allow(clippy::too_many_arguments)]
pub fn initialize_account_v2<S: std::hash::BuildHasher, F>(
    instruction_context: &InstructionContext,
    vote_account_index: IndexOfAccount,
    target_version: VoteStateTargetVersion,
    vote_init: &VoteInitV2,
    inflation_rewards_collector_index: IndexOfAccount,
    block_revenue_collector_index: IndexOfAccount,
    signers: &HashSet<Pubkey, S>,
    clock: &Clock,
    rent: &Rent,
    consume_pop_compute_units: F,
) -> Result<(), InstructionError>
where
    F: FnOnce() -> Result<(), InstructionError>,
{
    let mut vote_account =
        instruction_context.try_borrow_instruction_account(vote_account_index)?;
    VoteStateHandler::check_vote_account_length(&mut vote_account, target_version)?;
    let versioned = vote_account.get_state::<VoteStateVersions>()?;

    if !versioned.is_uninitialized() {
        return Err(InstructionError::AccountAlreadyInitialized);
    }

    // node must agree to accept this vote account
    verify_authorized_signer(&vote_init.node_pubkey, signers)?;

    // Per SIMD-0464, validate the collector accounts using the same checks as
    // `UpdateCommissionCollector` (SIMD-0232).
    let inflation_rewards_collector_key = validate_and_resolve_collector_key(
        instruction_context,
        &vote_account,
        inflation_rewards_collector_index,
        rent,
    )?;
    let block_revenue_collector_key = validate_and_resolve_collector_key(
        instruction_context,
        &vote_account,
        block_revenue_collector_index,
        rent,
    )?;

    // verify the BLS pubkey proof of possession
    verify_bls_proof_of_possession(
        vote_account.get_key(),
        &vote_init.authorized_voter_bls_pubkey,
        &vote_init.authorized_voter_bls_proof_of_possession,
        consume_pop_compute_units,
    )?;

    VoteStateHandler::init_vote_account_state_v2(
        &mut vote_account,
        vote_init,
        &inflation_rewards_collector_key,
        &block_revenue_collector_key,
        clock,
        target_version,
    )
}

/// Initialize the vote_state for a vote account
/// Assumes that the account is being init as part of a account creation or balance transfer and
/// that the transaction must be signed by the staker's keys
pub fn initialize_account<S: std::hash::BuildHasher>(
    vote_account: &mut BorrowedInstructionAccount,
    target_version: VoteStateTargetVersion,
    vote_init: &VoteInit,
    signers: &HashSet<Pubkey, S>,
    clock: &Clock,
) -> Result<(), InstructionError> {
    VoteStateHandler::check_vote_account_length(vote_account, target_version)?;
    let versioned = vote_account.get_state::<VoteStateVersions>()?;

    if !versioned.is_uninitialized() {
        return Err(InstructionError::AccountAlreadyInitialized);
    }

    // node must agree to accept this vote account
    verify_authorized_signer(&vote_init.node_pubkey, signers)?;

    VoteStateHandler::init_vote_account_state(vote_account, vote_init, clock, target_version)
}

pub fn create_v4_account_with_authorized(
    node_pubkey: &Pubkey,
    authorized_voter: &Pubkey,
    authorized_voter_bls_pubkey: [u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE],
    authorized_withdrawer: &Pubkey,
    inflation_rewards_commission_bps: u16,
    inflation_rewards_collector: &Pubkey,
    block_revenue_commission_bps: u16,
    block_revenue_collector: &Pubkey,
    lamports: u64,
) -> AccountSharedData {
    let mut vote_account = AccountSharedData::new(lamports, VoteStateV4::size_of(), &id());

    // PoP is stubbed here, since creation of an account assumes the account
    // was already initialized via `IntializeAccount` or `InitializeAccountV2`.
    let authorized_voter_bls_proof_of_possession = [0; BLS_PROOF_OF_POSSESSION_COMPRESSED_SIZE];

    let vote_state = VoteStateV4::new(
        &VoteInitV2 {
            node_pubkey: *node_pubkey,
            authorized_voter: *authorized_voter,
            authorized_voter_bls_pubkey,
            authorized_voter_bls_proof_of_possession,
            authorized_withdrawer: *authorized_withdrawer,
            inflation_rewards_commission_bps,
            block_revenue_commission_bps,
        },
        inflation_rewards_collector,
        block_revenue_collector,
        &Clock::default(),
    );

    VoteStateV4::serialize(
        &VoteStateVersions::V4(Box::new(vote_state)),
        vote_account.data_as_mut_slice(),
    )
    .unwrap();

    vote_account
}

pub fn create_bls_pubkey_and_proof_of_possession(
    vote_account_pubkey: &Pubkey,
) -> (
    [u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE],
    [u8; BLS_PROOF_OF_POSSESSION_COMPRESSED_SIZE],
) {
    let bls_keypair = BLSKeypair::new();
    create_bls_proof_of_possession(vote_account_pubkey, &bls_keypair)
}

pub fn create_bls_proof_of_possession(
    vote_account_pubkey: &Pubkey,
    bls_keypair: &BLSKeypair,
) -> (
    [u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE],
    [u8; BLS_PROOF_OF_POSSESSION_COMPRESSED_SIZE],
) {
    let bls_pubkey_bytes = bls_keypair.public.to_bytes_compressed();
    let message = generate_pop_message(vote_account_pubkey);

    let proof_of_possession = bls_keypair.proof_of_possession(Some(&message));
    let proof_of_possession_bytes = proof_of_possession.to_bytes_compressed();

    (bls_pubkey_bytes, proof_of_possession_bytes)
}

#[allow(clippy::arithmetic_side_effects)]
#[cfg(test)]
mod tests {
    use {
        super::*,
        assert_matches::assert_matches,
        solana_account::{AccountSharedData, ReadableAccount},
        solana_clock::DEFAULT_SLOTS_PER_EPOCH,
        solana_transaction_context::{
            instruction_accounts::InstructionAccount, transaction::TransactionContext,
        },
        solana_vote_interface::authorized_voters::AuthorizedVoters,
        std::collections::VecDeque,
        test_case::{test_case, test_matrix},
    };

    fn vote_state_new_for_test(
        vote_pubkey: &Pubkey,
        target_version: VoteStateTargetVersion,
    ) -> VoteStateHandler {
        let auth_pubkey = solana_pubkey::new_rand();
        let vote_init = VoteInit {
            node_pubkey: solana_pubkey::new_rand(),
            authorized_voter: auth_pubkey,
            authorized_withdrawer: auth_pubkey,
            commission: 0,
        };
        let clock = Clock::default();

        match target_version {
            VoteStateTargetVersion::V4 => VoteStateHandler::new_v4(VoteStateV4::new_with_defaults(
                vote_pubkey,
                &vote_init,
                &clock,
            )),
        }
    }

    #[test_case(VoteStateTargetVersion::V4 ; "VoteStateV4")]
    fn test_vote_state_upgrade_from_1_14_11(target_version: VoteStateTargetVersion) {
        let vote_pubkey = solana_pubkey::new_rand();
        let mut vote_state = vote_state_new_for_test(&vote_pubkey, target_version);

        // Simulate prior epochs completed with credits and each setting a new authorized voter
        vote_state.increment_credits(0, 100);
        assert_eq!(
            vote_state.set_new_authorized_voter(
                &solana_pubkey::new_rand(),
                0,
                1,
                None,
                |_pubkey| Ok(())
            ),
            Ok(())
        );
        vote_state.increment_credits(1, 200);
        assert_eq!(
            vote_state.set_new_authorized_voter(
                &solana_pubkey::new_rand(),
                1,
                2,
                None,
                |_pubkey| Ok(())
            ),
            Ok(())
        );
        vote_state.increment_credits(2, 300);
        assert_eq!(
            vote_state.set_new_authorized_voter(
                &solana_pubkey::new_rand(),
                2,
                3,
                None,
                |_pubkey| Ok(())
            ),
            Ok(())
        );

        // Simulate an account with an existing tower.
        vote_state.set_root_slot(Some(103));
        vote_state.set_votes(
            (104..136)
                .map(|slot| LandedVote {
                    latency: 0,
                    lockout: Lockout::new(slot),
                })
                .collect(),
        );

        // Create an initial vote account that is sized for the 1_14_11 version of vote state, and has only the
        // required lamports for rent exempt minimum at that size
        let vote_state_v1_14_11 = match target_version {
            VoteStateTargetVersion::V4 => {
                // v4 cannot be converted directly to V1_14_11.
                VoteState1_14_11 {
                    node_pubkey: *vote_state.node_pubkey(),
                    authorized_withdrawer: *vote_state.authorized_withdrawer(),
                    commission: vote_state.commission(),
                    votes: vote_state
                        .votes()
                        .iter()
                        .map(|landed_vote| (*landed_vote).into())
                        .collect(),
                    root_slot: vote_state.root_slot(),
                    authorized_voters: vote_state.authorized_voters().clone(),
                    epoch_credits: vote_state.epoch_credits().clone(),
                    last_timestamp: vote_state.last_timestamp().clone(),
                    prior_voters: CircBuf::default(), // v4 does not store prior_voters
                }
            }
        };
        let version1_14_11_serialized =
            bincode::serialize(&VoteStateVersions::V1_14_11(Box::new(vote_state_v1_14_11)))
                .unwrap();
        let version1_14_11_serialized_len = version1_14_11_serialized.len();
        let rent = Rent::default();
        let lamports = rent.minimum_balance(version1_14_11_serialized_len);
        let mut vote_account =
            AccountSharedData::new(lamports, version1_14_11_serialized_len, &id());
        vote_account.set_data_from_slice(&version1_14_11_serialized);

        // Create a fake TransactionContext with a fake InstructionContext with a single account which is the
        // vote account that was just created
        let processor_account = AccountSharedData::new(0, 0, &solana_sdk_ids::native_loader::id());
        let mut transaction_context = TransactionContext::new(
            vec![(id(), processor_account), (vote_pubkey, vote_account)],
            rent.clone(),
            0,
            0,
            1,
        );
        transaction_context
            .configure_top_level_instruction_for_tests(
                0,
                vec![InstructionAccount::new(1, false, true)],
                vec![],
            )
            .unwrap();
        let instruction_context = transaction_context.get_next_instruction_context().unwrap();

        // Get the BorrowedAccount from the InstructionContext which is what is used to manipulate and inspect account
        // state
        let mut borrowed_account = instruction_context
            .try_borrow_instruction_account(0)
            .unwrap();

        // Ensure that the vote state started out at 1_14_11
        let vote_state_version = borrowed_account.get_state::<VoteStateVersions>().unwrap();
        assert_matches!(vote_state_version, VoteStateVersions::V1_14_11(_));

        // Convert the vote state to current as would occur during vote instructions
        let converted_vote_state =
            get_vote_state_handler_checked(&borrowed_account, target_version).unwrap();

        // Check to make sure that the vote_state is unchanged
        assert!(vote_state == converted_vote_state);

        let vote_state = converted_vote_state;

        // Now re-set the vote account state, knowing the account only has
        // enough lamports for V1_14_11.
        match target_version {
            VoteStateTargetVersion::V4 => {
                // V4 will throw an error.
                assert_eq!(
                    vote_state
                        .clone()
                        .set_vote_account_state(&mut borrowed_account),
                    Err(InstructionError::AccountNotRentExempt)
                );
            }
        }

        // Convert the vote state to current as would occur during vote instructions
        let converted_vote_state =
            get_vote_state_handler_checked(&borrowed_account, target_version).unwrap();

        // Check to make sure that the vote_state is unchanged
        assert!(vote_state == converted_vote_state);

        let vote_state = converted_vote_state;

        // Now top-up the vote account's lamports to be rent exempt for the target version.
        let space = match target_version {
            VoteStateTargetVersion::V4 => VoteStateV4::size_of(),
        };
        assert_eq!(
            borrowed_account.set_lamports(rent.minimum_balance(space)),
            Ok(())
        );
        assert_eq!(
            vote_state
                .clone()
                .set_vote_account_state(&mut borrowed_account),
            Ok(())
        );

        // The vote state version should match the target version.
        let vote_state_version = borrowed_account.get_state::<VoteStateVersions>().unwrap();
        match target_version {
            VoteStateTargetVersion::V4 => {
                assert_matches!(vote_state_version, VoteStateVersions::V4(_));
            }
        }

        // Convert the vote state to current as would occur during vote instructions
        let converted_vote_state =
            get_vote_state_handler_checked(&borrowed_account, target_version).unwrap();

        // Check to make sure that the vote_state is unchanged
        assert_eq!(vote_state, converted_vote_state);
    }

    #[test_matrix(
        [VoteStateTargetVersion::V4],
        [true, false]
    )]
    fn test_update_commission(
        target_version: VoteStateTargetVersion,
        disable_commission_update_rule: bool,
    ) {
        let mut vote_state = vote_state_new_for_test(&solana_pubkey::new_rand(), target_version);
        let node_pubkey = *vote_state.node_pubkey();
        let withdrawer_pubkey = *vote_state.authorized_withdrawer();

        // Set commission to start.
        vote_state.set_commission(10);

        let serialized = vote_state.serialize();
        let serialized_len = serialized.len();
        let rent = Rent::default();
        let lamports = rent.minimum_balance(serialized_len);
        let mut vote_account = AccountSharedData::new(lamports, serialized_len, &id());
        vote_account.set_data_from_slice(&serialized);

        // Create a fake TransactionContext with a fake InstructionContext with a single account which is the
        // vote account that was just created
        let processor_account = AccountSharedData::new(0, 0, &solana_sdk_ids::native_loader::id());
        let mut transaction_context = TransactionContext::new(
            vec![(id(), processor_account), (node_pubkey, vote_account)],
            rent,
            0,
            0,
            1,
        );
        transaction_context
            .configure_top_level_instruction_for_tests(
                0,
                vec![InstructionAccount::new(1, false, true)],
                vec![],
            )
            .unwrap();
        let instruction_context = transaction_context.get_next_instruction_context().unwrap();

        // Get the BorrowedAccount from the InstructionContext which is what is used to manipulate and inspect account
        // state
        let mut borrowed_account = instruction_context
            .try_borrow_instruction_account(0)
            .unwrap();

        let epoch_schedule = std::sync::Arc::new(EpochSchedule::without_warmup());

        let first_half_clock = std::sync::Arc::new(Clock {
            slot: epoch_schedule.slots_per_epoch / 4,
            ..Clock::default()
        });

        let second_half_clock = std::sync::Arc::new(Clock {
            slot: (epoch_schedule.slots_per_epoch * 3) / 4,
            ..Clock::default()
        });

        let signers: HashSet<Pubkey> = vec![withdrawer_pubkey].into_iter().collect();

        // Increase commission in first half of epoch -- allowed
        assert_eq!(
            get_vote_state_handler_checked(&borrowed_account, target_version,)
                .unwrap()
                .commission(),
            10
        );
        assert_matches!(
            update_commission(
                &mut borrowed_account,
                target_version,
                11,
                &signers,
                &epoch_schedule,
                &first_half_clock,
                disable_commission_update_rule,
            ),
            Ok(())
        );
        assert_eq!(
            get_vote_state_handler_checked(&borrowed_account, target_version,)
                .unwrap()
                .commission(),
            11
        );

        // Increase commission in second half of epoch -- disallowed if update rule is enabled
        let result = update_commission(
            &mut borrowed_account,
            target_version,
            12,
            &signers,
            &epoch_schedule,
            &second_half_clock,
            disable_commission_update_rule,
        );
        let state_commission = get_vote_state_handler_checked(&borrowed_account, target_version)
            .unwrap()
            .commission();
        if disable_commission_update_rule {
            assert_matches!(result, Ok(()));
            assert_eq!(state_commission, 12);
        } else {
            assert_matches!(result, Err(_));
            assert_eq!(state_commission, 11);
        }

        // Decrease commission in first half of epoch -- always allowed
        assert_matches!(
            update_commission(
                &mut borrowed_account,
                target_version,
                10,
                &signers,
                &epoch_schedule,
                &first_half_clock,
                disable_commission_update_rule,
            ),
            Ok(())
        );
        assert_eq!(
            get_vote_state_handler_checked(&borrowed_account, target_version,)
                .unwrap()
                .commission(),
            10
        );

        assert_eq!(
            get_vote_state_handler_checked(&borrowed_account, target_version,)
                .unwrap()
                .commission(),
            10
        );

        // Decrease commission in second half of epoch -- always allowed
        assert_matches!(
            update_commission(
                &mut borrowed_account,
                target_version,
                9,
                &signers,
                &epoch_schedule,
                &second_half_clock,
                disable_commission_update_rule,
            ),
            Ok(())
        );
        assert_eq!(
            get_vote_state_handler_checked(&borrowed_account, target_version,)
                .unwrap()
                .commission(),
            9
        );
    }

    /// Test update_commission_bps (SIMD-0291).
    ///
    /// Unlike test_update_commission, SIMD-0291 has no timing restrictions
    /// (per SIMD-0249). Updates are always allowed regardless of epoch position.
    ///
    /// This test only uses V4 since SIMD-0291 depends on SIMD-0185 (VoteStateV4).
    #[test]
    fn test_update_commission_bps() {
        let target_version = VoteStateTargetVersion::V4;
        let mut vote_state = vote_state_new_for_test(&solana_pubkey::new_rand(), target_version);
        let withdrawer_pubkey = *vote_state.authorized_withdrawer();
        let node_pubkey = *vote_state.node_pubkey();

        // Set initial commission.
        vote_state.set_commission(10); // 10%

        let serialized = vote_state.serialize();
        let serialized_len = serialized.len();
        let rent = Rent::default();
        let lamports = rent.minimum_balance(serialized_len);
        let mut vote_account = AccountSharedData::new(lamports, serialized_len, &id());
        vote_account.set_data_from_slice(&serialized);

        let processor_account = AccountSharedData::new(0, 0, &solana_sdk_ids::native_loader::id());
        let mut transaction_context = TransactionContext::new(
            vec![(id(), processor_account), (node_pubkey, vote_account)],
            rent,
            0,
            0,
            1,
        );
        transaction_context
            .configure_top_level_instruction_for_tests(
                0,
                vec![InstructionAccount::new(1, false, true)],
                vec![],
            )
            .unwrap();
        let instruction_context = transaction_context.get_next_instruction_context().unwrap();
        let mut borrowed_account = instruction_context
            .try_borrow_instruction_account(0)
            .unwrap();

        let signers: HashSet<Pubkey> = vec![withdrawer_pubkey].into_iter().collect();
        let non_signers: HashSet<Pubkey> = HashSet::new();

        // `CommissionKind::BlockRevenue` returns `InvalidInstructionData` when
        // block_revenue_sharing is disabled.
        assert_eq!(
            update_commission_bps(
                &mut borrowed_account,
                target_version,
                500,
                CommissionKind::BlockRevenue,
                &signers,
                false, // block_revenue_sharing disabled
            ),
            Err(InstructionError::InvalidInstructionData)
        );

        // Missing signature returns `MissingRequiredSignature`.
        assert_eq!(
            update_commission_bps(
                &mut borrowed_account,
                target_version,
                500,
                CommissionKind::InflationRewards,
                &non_signers,
                false,
            ),
            Err(InstructionError::MissingRequiredSignature)
        );

        // Incorrect signature for withdraw authority returns `MissingRequiredSignature`.
        let wrong_signers: HashSet<Pubkey> = vec![Pubkey::new_unique()].into_iter().collect();
        assert_eq!(
            update_commission_bps(
                &mut borrowed_account,
                target_version,
                500,
                CommissionKind::InflationRewards,
                &wrong_signers,
                false,
            ),
            Err(InstructionError::MissingRequiredSignature)
        );

        let mut commission_bps_roundtrip = |new_commission_bps: u16| {
            update_commission_bps(
                &mut borrowed_account,
                target_version,
                new_commission_bps,
                CommissionKind::InflationRewards,
                &signers,
                false,
            )
            .unwrap();
            update_commission_bps(
                &mut borrowed_account,
                target_version,
                new_commission_bps,
                CommissionKind::BlockRevenue,
                &signers,
                true,
            )
            .unwrap();
            let handler =
                get_vote_state_handler_checked(&borrowed_account, target_version).unwrap();
            assert_eq!(
                handler.as_ref_v4().inflation_rewards_commission_bps,
                new_commission_bps
            );
            assert_eq!(
                handler.as_ref_v4().block_revenue_commission_bps,
                new_commission_bps
            );
        };

        // There's no timing check for SIMD-0291, so just go back and forth
        // with new values.

        commission_bps_roundtrip(1_100); // Increase to 11%
        commission_bps_roundtrip(5_000); // Increase to 50%
        commission_bps_roundtrip(4_400); // Decrease to 44%
        commission_bps_roundtrip(4_600); // Increase to 46%

        // Values > 10,000 bps are allowed at program level.
        commission_bps_roundtrip(15_000); // 150%
        commission_bps_roundtrip(50_000); // 500%
    }

    #[test_case(0, true; "first slot")]
    #[test_case(DEFAULT_SLOTS_PER_EPOCH / 2, true; "halfway through epoch")]
    #[test_case((DEFAULT_SLOTS_PER_EPOCH / 2).saturating_add(1), false; "halfway through epoch plus one")]
    #[test_case(DEFAULT_SLOTS_PER_EPOCH.saturating_sub(1), false; "last slot in epoch")]
    #[test_case(DEFAULT_SLOTS_PER_EPOCH, true; "first slot in second epoch")]
    fn test_epoch_half_check(slot: Slot, expected_allowed: bool) {
        let epoch_schedule = EpochSchedule::without_warmup();
        assert_eq!(
            is_commission_update_allowed(slot, &epoch_schedule),
            expected_allowed
        );
    }

    #[test]
    fn test_warmup_epoch_half_check_with_warmup() {
        let epoch_schedule = EpochSchedule::default();
        let first_normal_slot = epoch_schedule.first_normal_slot;
        // first slot works
        assert!(is_commission_update_allowed(0, &epoch_schedule));
        // right before first normal slot works, since all warmup slots allow
        // commission updates
        assert!(is_commission_update_allowed(
            first_normal_slot - 1,
            &epoch_schedule
        ));
    }

    #[test_case(0, true; "first slot")]
    #[test_case(DEFAULT_SLOTS_PER_EPOCH / 2, true; "halfway through epoch")]
    #[test_case((DEFAULT_SLOTS_PER_EPOCH / 2).saturating_add(1), false; "halfway through epoch plus one")]
    #[test_case(DEFAULT_SLOTS_PER_EPOCH.saturating_sub(1), false; "last slot in epoch")]
    #[test_case(DEFAULT_SLOTS_PER_EPOCH, true; "first slot in second epoch")]
    fn test_epoch_half_check_with_warmup(slot: Slot, expected_allowed: bool) {
        let epoch_schedule = EpochSchedule::default();
        let first_normal_slot = epoch_schedule.first_normal_slot;
        assert_eq!(
            is_commission_update_allowed(first_normal_slot.saturating_add(slot), &epoch_schedule),
            expected_allowed
        );
    }

    #[test]
    fn test_create_v4_account_with_authorized() {
        let node_pubkey = Pubkey::new_unique();
        let authorized_voter = Pubkey::new_unique();
        let authorized_withdrawer = Pubkey::new_unique();
        let bls_pubkey_compressed = [42; 48];
        let inflation_rewards_commission_bps = 10000;
        let lamports = 100;
        let vote_account = create_v4_account_with_authorized(
            &node_pubkey,
            &authorized_voter,
            bls_pubkey_compressed,
            &authorized_withdrawer,
            inflation_rewards_commission_bps,
            &authorized_withdrawer,
            0,
            &node_pubkey,
            lamports,
        );
        assert_eq!(vote_account.lamports(), lamports);
        assert_eq!(vote_account.owner(), &id());
        assert_eq!(vote_account.data().len(), VoteStateV4::size_of());
        let vote_state_v4 = VoteStateV4::deserialize(vote_account.data(), &node_pubkey).unwrap();
        assert_eq!(vote_state_v4.node_pubkey, node_pubkey);
        assert_eq!(
            vote_state_v4.authorized_voters,
            AuthorizedVoters::new(0, authorized_voter)
        );
        assert_eq!(vote_state_v4.authorized_withdrawer, authorized_withdrawer);
        assert_eq!(
            vote_state_v4.bls_pubkey_compressed,
            Some(bls_pubkey_compressed)
        );
        assert_eq!(
            vote_state_v4.inflation_rewards_commission_bps,
            inflation_rewards_commission_bps
        );
    }

    #[test]
    fn test_update_validator_identity_syncs_block_revenue_collector() {
        // Feature disabled; block revenue collector should always sync.
        let custom_commission_collector_enabled = false;

        let vote_state =
            vote_state_new_for_test(&solana_pubkey::new_rand(), VoteStateTargetVersion::V4);
        let node_pubkey = *vote_state.node_pubkey();
        let withdrawer_pubkey = *vote_state.authorized_withdrawer();

        let serialized = vote_state.serialize();
        let serialized_len = serialized.len();
        let rent = Rent::default();
        let lamports = rent.minimum_balance(serialized_len);
        let mut vote_account = AccountSharedData::new(lamports, serialized_len, &id());
        vote_account.set_data_from_slice(&serialized);

        let processor_account = AccountSharedData::new(0, 0, &solana_sdk_ids::native_loader::id());
        let mut transaction_context = TransactionContext::new(
            vec![(id(), processor_account), (node_pubkey, vote_account)],
            rent,
            0,
            0,
            1,
        );
        transaction_context
            .configure_top_level_instruction_for_tests(
                0,
                vec![InstructionAccount::new(1, false, true)],
                vec![],
            )
            .unwrap();
        let instruction_context = transaction_context.get_next_instruction_context().unwrap();
        let mut borrowed_account = instruction_context
            .try_borrow_instruction_account(0)
            .unwrap();

        let new_node_pubkey = solana_pubkey::new_rand();
        let signers: HashSet<Pubkey> = vec![withdrawer_pubkey, new_node_pubkey]
            .into_iter()
            .collect();

        update_validator_identity(
            &mut borrowed_account,
            VoteStateTargetVersion::V4,
            &new_node_pubkey,
            &signers,
            custom_commission_collector_enabled,
        )
        .unwrap();

        // Both `node_pubkey` and `block_revenue_collector` should be set to
        // the new node pubkey.
        let vote_state =
            VoteStateV4::deserialize(borrowed_account.get_data(), &new_node_pubkey).unwrap();
        assert_eq!(vote_state.node_pubkey, new_node_pubkey);
        assert_eq!(vote_state.block_revenue_collector, new_node_pubkey);

        // Run it again.
        let new_node_pubkey = solana_pubkey::new_rand();
        let signers: HashSet<Pubkey> = vec![withdrawer_pubkey, new_node_pubkey]
            .into_iter()
            .collect();

        update_validator_identity(
            &mut borrowed_account,
            VoteStateTargetVersion::V4,
            &new_node_pubkey,
            &signers,
            custom_commission_collector_enabled,
        )
        .unwrap();

        let vote_state =
            VoteStateV4::deserialize(borrowed_account.get_data(), &new_node_pubkey).unwrap();
        assert_eq!(vote_state.node_pubkey, new_node_pubkey);
        assert_eq!(vote_state.block_revenue_collector, new_node_pubkey);
    }

    #[test]
    fn test_update_validator_identity_preserves_custom_block_revenue_collector() {
        // SIMD-0232 enabled.
        //
        // Once a validator has set a custom block_revenue_collector, rotating
        // the validator identity via UpdateValidatorIdentity must NOT clobber
        // the custom collector.
        let custom_commission_collector_enabled = true;

        let vote_pubkey = solana_pubkey::new_rand();
        let mut vote_state = vote_state_new_for_test(&vote_pubkey, VoteStateTargetVersion::V4);
        let node_pubkey = *vote_state.node_pubkey();
        let withdrawer_pubkey = *vote_state.authorized_withdrawer();

        // Seed a custom block_revenue_collector distinct from the node identity.
        let custom_collector = solana_pubkey::new_rand();
        vote_state.set_block_revenue_collector(custom_collector);

        let serialized = vote_state.serialize();
        let serialized_len = serialized.len();
        let rent = Rent::default();
        let lamports = rent.minimum_balance(serialized_len);
        let mut vote_account = AccountSharedData::new(lamports, serialized_len, &id());
        vote_account.set_data_from_slice(&serialized);

        let processor_account = AccountSharedData::new(0, 0, &solana_sdk_ids::native_loader::id());
        let mut transaction_context = TransactionContext::new(
            vec![(id(), processor_account), (node_pubkey, vote_account)],
            rent,
            0,
            0,
            1,
        );
        transaction_context
            .configure_top_level_instruction_for_tests(
                0,
                vec![InstructionAccount::new(1, false, true)],
                vec![],
            )
            .unwrap();
        let instruction_context = transaction_context.get_next_instruction_context().unwrap();
        let mut borrowed_account = instruction_context
            .try_borrow_instruction_account(0)
            .unwrap();

        let new_node_pubkey = solana_pubkey::new_rand();
        let signers: HashSet<Pubkey> = vec![withdrawer_pubkey, new_node_pubkey]
            .into_iter()
            .collect();

        update_validator_identity(
            &mut borrowed_account,
            VoteStateTargetVersion::V4,
            &new_node_pubkey,
            &signers,
            custom_commission_collector_enabled,
        )
        .unwrap();

        // node_pubkey updated, but block_revenue_collector preserved.
        let vote_state =
            VoteStateV4::deserialize(borrowed_account.get_data(), &new_node_pubkey).unwrap();
        assert_eq!(vote_state.node_pubkey, new_node_pubkey);
        assert_eq!(vote_state.block_revenue_collector, custom_collector);
        assert_ne!(vote_state.block_revenue_collector, new_node_pubkey);
    }

    #[test]
    fn test_get_and_update_authorized_voter_v4_with_bls() {
        let vote_account_pubkey = Pubkey::new_unique();
        let (bls_pubkey, bls_proof_of_possession) =
            create_bls_pubkey_and_proof_of_possession(&vote_account_pubkey);
        let node_pubkey = Pubkey::new_unique();
        let authorized_voter = Pubkey::new_unique();
        let authorized_withdrawer = Pubkey::new_unique();
        let inflation_rewards_commission_bps = 10000;
        let rent = Rent::default();
        let lamports = rent.minimum_balance(VoteStateV4::size_of());
        // Create a VoteStateV4 account without BLS pubkey
        let vote_account = create_v4_account_with_authorized(
            &node_pubkey,
            &authorized_voter,
            [0u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE],
            &authorized_withdrawer,
            inflation_rewards_commission_bps,
            &authorized_withdrawer,
            0,
            &node_pubkey,
            lamports,
        );
        assert_eq!(vote_account.lamports(), lamports);
        assert_eq!(vote_account.owner(), &id());
        assert_eq!(vote_account.data().len(), VoteStateV4::size_of());

        let processor_account = AccountSharedData::new(0, 0, &solana_sdk_ids::native_loader::id());
        let mut transaction_context = TransactionContext::new(
            vec![
                (id(), processor_account),
                (vote_account_pubkey, vote_account),
            ],
            rent,
            0,
            0,
            1,
        );
        transaction_context
            .configure_top_level_instruction_for_tests(
                0,
                vec![InstructionAccount::new(1, false, true)],
                vec![],
            )
            .unwrap();
        let instruction_context = transaction_context.get_next_instruction_context().unwrap();
        let mut borrowed_account = instruction_context
            .try_borrow_instruction_account(0)
            .unwrap();

        let new_node_pubkey = solana_pubkey::new_rand();
        let signers: HashSet<Pubkey> = vec![authorized_withdrawer, new_node_pubkey]
            .into_iter()
            .collect();
        let clock = Clock::default();
        assert!(
            authorize(
                &mut borrowed_account,
                VoteStateTargetVersion::V4,
                &new_node_pubkey,
                VoteAuthorize::VoterWithBLS(VoterWithBLSArgs {
                    bls_pubkey,
                    bls_proof_of_possession
                }),
                &signers,
                &clock,
                true,
                || Ok(()),
            )
            .is_ok()
        );
        let vote_state = VoteStateHandler::new_v4(
            VoteStateV4::deserialize(borrowed_account.get_data(), &new_node_pubkey).unwrap(),
        );
        assert_eq!(
            vote_state.as_ref_v4().bls_pubkey_compressed,
            Some(bls_pubkey)
        );
        assert!(vote_state.has_bls_pubkey());

        // Test replay attack, can't use someone else's BLS pubkey and PoP
        let clock = Clock {
            epoch: 3,
            ..Clock::default()
        };
        let (others_bls_pubkey, others_bls_proof_of_possession) =
            create_bls_pubkey_and_proof_of_possession(&Pubkey::new_unique());
        let new_node_pubkey = solana_pubkey::new_rand();
        let signers: HashSet<Pubkey> = vec![authorized_withdrawer, new_node_pubkey]
            .into_iter()
            .collect();
        assert_eq!(
            authorize(
                &mut borrowed_account,
                VoteStateTargetVersion::V4,
                &new_node_pubkey,
                VoteAuthorize::VoterWithBLS(VoterWithBLSArgs {
                    bls_pubkey: others_bls_pubkey,
                    bls_proof_of_possession: others_bls_proof_of_possession
                }),
                &signers,
                &clock,
                true,
                || Ok(()),
            ),
            Err(InstructionError::InvalidArgument),
        );

        // Test updating to a new BLS pubkey, can only do it in next epoch.
        let clock = Clock {
            epoch: 5,
            ..Clock::default()
        };
        let (new_bls_pubkey, new_bls_proof_of_possession) =
            create_bls_pubkey_and_proof_of_possession(&vote_account_pubkey);
        let new_authorized_voter = solana_pubkey::new_rand();
        let signers: HashSet<Pubkey> = vec![authorized_withdrawer, new_authorized_voter]
            .into_iter()
            .collect();
        assert_eq!(
            authorize(
                &mut borrowed_account,
                VoteStateTargetVersion::V4,
                &new_authorized_voter,
                VoteAuthorize::VoterWithBLS(VoterWithBLSArgs {
                    bls_pubkey: new_bls_pubkey,
                    bls_proof_of_possession: new_bls_proof_of_possession
                }),
                &signers,
                &clock,
                true,
                || Ok(()),
            ),
            Ok(())
        );
        let vote_state = VoteStateHandler::new_v4(
            VoteStateV4::deserialize(borrowed_account.get_data(), &new_authorized_voter).unwrap(),
        );
        assert_eq!(
            vote_state.as_ref_v4().bls_pubkey_compressed,
            Some(new_bls_pubkey)
        );
        assert!(vote_state.has_bls_pubkey());
    }

    fn new_transaction_context(
        accounts: Vec<(Pubkey, AccountSharedData)>,
        instruction_accounts: Vec<InstructionAccount>,
        rent: &Rent,
    ) -> TransactionContext<'_> {
        let mut transaction_context = TransactionContext::new(accounts, rent.clone(), 0, 0, 1);
        transaction_context
            .configure_top_level_instruction_for_tests(0, instruction_accounts, vec![])
            .unwrap();
        transaction_context
    }

    #[test]
    fn test_validate_and_resolve_collector_key() {
        let rent = Rent::default();
        let processor_account = AccountSharedData::new(0, 0, &solana_sdk_ids::native_loader::id());
        let vote_pubkey = solana_pubkey::new_rand();
        let vote_account = AccountSharedData::new(1, 0, &id());
        let collector_pubkey = solana_pubkey::new_rand();
        let valid_collector =
            || AccountSharedData::new(rent.minimum_balance(0), 0, &system_program::id());

        // Success: VoteAccount variant returns the vote account's key.
        {
            let transaction_context = new_transaction_context(
                vec![
                    (id(), processor_account.clone()),
                    (vote_pubkey, vote_account.clone()),
                ],
                vec![InstructionAccount::new(1, false, true)],
                &rent,
            );
            let instruction_context = transaction_context.get_next_instruction_context().unwrap();
            let borrowed_vote = instruction_context
                .try_borrow_instruction_account(0)
                .unwrap();
            assert_eq!(
                validate_and_resolve_collector_key(&instruction_context, &borrowed_vote, 0, &rent),
                Ok(vote_pubkey),
            );
        }

        // Success: NewAccount (happy) path returns the collector's key.
        {
            let transaction_context = new_transaction_context(
                vec![
                    (id(), processor_account.clone()),
                    (vote_pubkey, vote_account.clone()),
                    (collector_pubkey, valid_collector()),
                ],
                vec![
                    InstructionAccount::new(1, false, true),
                    InstructionAccount::new(2, false, true),
                ],
                &rent,
            );
            let instruction_context = transaction_context.get_next_instruction_context().unwrap();
            let borrowed_vote = instruction_context
                .try_borrow_instruction_account(0)
                .unwrap();
            assert_eq!(
                validate_and_resolve_collector_key(&instruction_context, &borrowed_vote, 1, &rent),
                Ok(collector_pubkey),
            );
        }

        // Success: Incinerator is an accepted collector (SIMD-0232 explicitly
        // allows it — funds sent to the incinerator are burned at end-of-block).
        {
            // Imagine the incinerator is temporarily holding funds at the time
            // of the invocation.
            let incinerator_account =
                AccountSharedData::new(rent.minimum_balance(0), 0, &system_program::id());
            let transaction_context = new_transaction_context(
                vec![
                    (id(), processor_account.clone()),
                    (vote_pubkey, vote_account.clone()),
                    (solana_sdk_ids::incinerator::id(), incinerator_account),
                ],
                vec![
                    InstructionAccount::new(1, false, true),
                    InstructionAccount::new(2, false, true),
                ],
                &rent,
            );
            let instruction_context = transaction_context.get_next_instruction_context().unwrap();
            let borrowed_vote = instruction_context
                .try_borrow_instruction_account(0)
                .unwrap();
            assert_eq!(
                validate_and_resolve_collector_key(&instruction_context, &borrowed_vote, 1, &rent),
                Ok(solana_sdk_ids::incinerator::id()),
            );
        }

        // Fail: Collector account not system-owned.
        {
            let bad_owner =
                AccountSharedData::new(rent.minimum_balance(0), 0, &solana_pubkey::new_rand());
            let transaction_context = new_transaction_context(
                vec![
                    (id(), processor_account.clone()),
                    (vote_pubkey, vote_account.clone()),
                    (collector_pubkey, bad_owner),
                ],
                vec![
                    InstructionAccount::new(1, false, true),
                    InstructionAccount::new(2, false, true),
                ],
                &rent,
            );
            let instruction_context = transaction_context.get_next_instruction_context().unwrap();
            let borrowed_vote = instruction_context
                .try_borrow_instruction_account(0)
                .unwrap();
            assert_eq!(
                validate_and_resolve_collector_key(&instruction_context, &borrowed_vote, 1, &rent),
                Err(InstructionError::InvalidAccountOwner),
            );
        }

        // Fail: Collector account not rent-exempt.
        {
            let underfunded = AccountSharedData::new(0, 0, &system_program::id());
            let transaction_context = new_transaction_context(
                vec![
                    (id(), processor_account.clone()),
                    (vote_pubkey, vote_account.clone()),
                    (collector_pubkey, underfunded),
                ],
                vec![
                    InstructionAccount::new(1, false, true),
                    InstructionAccount::new(2, false, true),
                ],
                &rent,
            );
            let instruction_context = transaction_context.get_next_instruction_context().unwrap();
            let borrowed_vote = instruction_context
                .try_borrow_instruction_account(0)
                .unwrap();
            assert_eq!(
                validate_and_resolve_collector_key(&instruction_context, &borrowed_vote, 1, &rent),
                Err(InstructionError::InsufficientFunds),
            );
        }

        // Fail: Collector account not writable (reserved account check).
        {
            let transaction_context = new_transaction_context(
                vec![
                    (id(), processor_account),
                    (vote_pubkey, vote_account),
                    (collector_pubkey, valid_collector()),
                ],
                vec![
                    InstructionAccount::new(1, false, true),
                    InstructionAccount::new(2, false, false), // <-- Not writable
                ],
                &rent,
            );
            let instruction_context = transaction_context.get_next_instruction_context().unwrap();
            let borrowed_vote = instruction_context
                .try_borrow_instruction_account(0)
                .unwrap();
            assert_eq!(
                validate_and_resolve_collector_key(&instruction_context, &borrowed_vote, 1, &rent),
                Err(InstructionError::InvalidArgument),
            );
        }
    }

    /// Test update_commission_collector (SIMD-0232).
    ///
    /// This test only uses V4 since SIMD-0232 depends on SIMD-0185 (VoteStateV4).
    #[test]
    fn test_update_commission_collector() {
        let target_version = VoteStateTargetVersion::V4;
        let vote_pubkey = solana_pubkey::new_rand();
        let vote_state = vote_state_new_for_test(&vote_pubkey, target_version);
        let withdrawer_pubkey = *vote_state.authorized_withdrawer();
        let node_pubkey = *vote_state.node_pubkey();

        let signers: HashSet<Pubkey> = vec![withdrawer_pubkey].into_iter().collect();

        let serialized = vote_state.serialize();
        let serialized_len = serialized.len();
        let rent = Rent::default();
        let lamports = rent.minimum_balance(serialized_len);
        let mut vote_account = AccountSharedData::new(lamports, serialized_len, &id());
        vote_account.set_data_from_slice(&serialized);

        let get_commission_collector =
            |instruction_context: &InstructionContext, kind: CommissionKind| {
                let vote_account = instruction_context
                    .try_borrow_instruction_account(0)
                    .unwrap();
                let handler =
                    get_vote_state_handler_checked(&vote_account, target_version).unwrap();
                let vote_state = handler.as_ref_v4();
                match kind {
                    CommissionKind::InflationRewards => vote_state.inflation_rewards_collector,
                    CommissionKind::BlockRevenue => vote_state.block_revenue_collector,
                }
            };

        let processor_account = AccountSharedData::new(0, 0, &solana_sdk_ids::native_loader::id());

        // Create a valid collector account (system-owned, rent-exempt).
        let new_collector = solana_pubkey::new_rand();
        let collector_lamports = rent.minimum_balance(0);
        let collector_account =
            AccountSharedData::new(collector_lamports, 0, &system_program::id());

        let original_inflation_collector = vote_pubkey;
        let original_block_revenue_collector = node_pubkey;

        // Should pass.
        {
            let transaction_context = new_transaction_context(
                vec![
                    (id(), processor_account.clone()),
                    (vote_pubkey, vote_account.clone()),
                    (new_collector, collector_account.clone()),
                ],
                vec![
                    InstructionAccount::new(1, false, true),
                    InstructionAccount::new(2, false, true),
                ],
                &rent,
            );
            let instruction_context = transaction_context.get_next_instruction_context().unwrap();
            // InflationRewards kind.
            update_commission_collector(
                &instruction_context,
                0,
                target_version,
                1,
                CommissionKind::InflationRewards,
                &signers,
                &rent,
            )
            .unwrap();
            assert_eq!(
                get_commission_collector(&instruction_context, CommissionKind::InflationRewards),
                new_collector,
            );
            assert_eq!(
                get_commission_collector(&instruction_context, CommissionKind::BlockRevenue),
                original_block_revenue_collector, // Unchanged
            );

            // BlockRevenue kind.
            update_commission_collector(
                &instruction_context,
                0,
                target_version,
                1,
                CommissionKind::BlockRevenue,
                &signers,
                &rent,
            )
            .unwrap();
            assert_eq!(
                get_commission_collector(&instruction_context, CommissionKind::InflationRewards),
                new_collector,
            );
            assert_eq!(
                get_commission_collector(&instruction_context, CommissionKind::BlockRevenue),
                new_collector,
            );
        }

        // Should pass - setting collector to vote account.
        {
            let transaction_context = new_transaction_context(
                vec![
                    (id(), processor_account.clone()),
                    (vote_pubkey, vote_account.clone()),
                ],
                vec![
                    InstructionAccount::new(1, false, true),
                    InstructionAccount::new(1, false, true), // collector = vote account (aliased)
                ],
                &rent,
            );
            let instruction_context = transaction_context.get_next_instruction_context().unwrap();
            // InflationRewards kind.
            update_commission_collector(
                &instruction_context,
                0,
                target_version,
                1,
                CommissionKind::InflationRewards,
                &signers,
                &rent,
            )
            .unwrap();
            assert_eq!(
                get_commission_collector(&instruction_context, CommissionKind::InflationRewards),
                vote_pubkey,
            );
            assert_eq!(
                get_commission_collector(&instruction_context, CommissionKind::BlockRevenue),
                original_block_revenue_collector, // Unchanged
            );

            // BlockRevenue kind.
            update_commission_collector(
                &instruction_context,
                0,
                target_version,
                1,
                CommissionKind::BlockRevenue,
                &signers,
                &rent,
            )
            .unwrap();
            assert_eq!(
                get_commission_collector(&instruction_context, CommissionKind::InflationRewards),
                vote_pubkey,
            );
            assert_eq!(
                get_commission_collector(&instruction_context, CommissionKind::BlockRevenue),
                vote_pubkey,
            );
        }

        // V3 -> V4 auto-conversion side-effect: updating one collector against a
        // V3-serialized account causes the "other" collector to be written as
        // its V4 default (inflation_rewards_collector = vote_pubkey,
        // block_revenue_collector = node_pubkey), per try_convert_to_vote_state_v4.
        {
            let v3 = get_max_sized_vote_state_v3();
            let v3_node_pubkey = v3.node_pubkey;
            let v3_withdrawer = v3.authorized_withdrawer;
            let v3_vote_pubkey = solana_pubkey::new_rand();

            let v4_size = VoteStateV4::size_of();
            let mut account_data = vec![0u8; v4_size];
            bincode::serialize_into(&mut account_data[..], &VoteStateVersions::V3(Box::new(v3)))
                .unwrap();
            let mut v3_vote_account =
                AccountSharedData::new(rent.minimum_balance(v4_size), v4_size, &id());
            v3_vote_account.set_data_from_slice(&account_data);

            let v3_signers: HashSet<Pubkey> = vec![v3_withdrawer].into_iter().collect();

            let transaction_context = new_transaction_context(
                vec![
                    (id(), processor_account.clone()),
                    (v3_vote_pubkey, v3_vote_account),
                    (new_collector, collector_account.clone()),
                ],
                vec![
                    InstructionAccount::new(1, false, true),
                    InstructionAccount::new(2, false, true),
                ],
                &rent,
            );
            let instruction_context = transaction_context.get_next_instruction_context().unwrap();
            update_commission_collector(
                &instruction_context,
                0,
                target_version,
                1,
                CommissionKind::InflationRewards,
                &v3_signers,
                &rent,
            )
            .unwrap();

            // The updated field reflects the caller's new collector.
            assert_eq!(
                get_commission_collector(&instruction_context, CommissionKind::InflationRewards),
                new_collector,
            );
            // The *other* field was reset to its V4-conversion default
            // (block_revenue_collector = node_pubkey from the V3 source).
            assert_eq!(
                get_commission_collector(&instruction_context, CommissionKind::BlockRevenue),
                v3_node_pubkey,
            );
        }

        // Should fail - deserialization error paths.
        //
        // All four variants produce `InvalidAccountData`:
        // * V0_23_5 is explicitly unsupported
        // * V1_14_11, V3, and V4 fail if state is bad
        let run_with_account_data = |bytes: Vec<u8>| -> Result<(), InstructionError> {
            let mut bad_vote_account =
                AccountSharedData::new(rent.minimum_balance(bytes.len()), bytes.len(), &id());
            bad_vote_account.set_data_from_slice(&bytes);
            let transaction_context = new_transaction_context(
                vec![
                    (id(), processor_account.clone()),
                    (vote_pubkey, bad_vote_account),
                    (new_collector, collector_account.clone()),
                ],
                vec![
                    InstructionAccount::new(1, false, true),
                    InstructionAccount::new(2, false, true),
                ],
                &rent,
            );
            let instruction_context = transaction_context.get_next_instruction_context().unwrap();
            update_commission_collector(
                &instruction_context,
                0,
                target_version,
                1,
                CommissionKind::InflationRewards,
                &signers,
                &rent,
            )
        };
        let variant_with_short_body = |variant: u32| -> Vec<u8> {
            let mut bytes = vec![0u8; 8];
            bytes[..4].copy_from_slice(&variant.to_le_bytes());
            bytes
        };

        // Should fail - V0_23_5 not supported.
        assert_eq!(
            run_with_account_data(variant_with_short_body(0)),
            Err(InstructionError::InvalidAccountData),
        );

        // Should fail - Invalid V1_14_11 state.
        assert_eq!(
            run_with_account_data(variant_with_short_body(1)),
            Err(InstructionError::InvalidAccountData),
        );

        // Should fail - Invalid V3 state.
        assert_eq!(
            run_with_account_data(variant_with_short_body(2)),
            Err(InstructionError::InvalidAccountData),
        );

        // Should fail - Invalid V4 state.
        assert_eq!(
            run_with_account_data(variant_with_short_body(3)),
            Err(InstructionError::InvalidAccountData),
        );

        // Should fail - authorized withdrawer didn't sign.
        {
            let non_signers: HashSet<Pubkey> = HashSet::new();
            let transaction_context = new_transaction_context(
                vec![
                    (id(), processor_account.clone()),
                    (vote_pubkey, vote_account.clone()),
                    (new_collector, collector_account.clone()),
                ],
                vec![
                    InstructionAccount::new(1, false, true),
                    InstructionAccount::new(2, false, true),
                ],
                &rent,
            );
            let instruction_context = transaction_context.get_next_instruction_context().unwrap();
            assert_eq!(
                update_commission_collector(
                    &instruction_context,
                    0,
                    target_version,
                    1,
                    CommissionKind::InflationRewards,
                    &non_signers,
                    &rent,
                ),
                Err(InstructionError::MissingRequiredSignature)
            );
            assert_eq!(
                get_commission_collector(&instruction_context, CommissionKind::InflationRewards),
                original_inflation_collector, // Unchanged
            );
            assert_eq!(
                get_commission_collector(&instruction_context, CommissionKind::BlockRevenue),
                original_block_revenue_collector, // Unchanged
            );
        }

        // Should fail - wrong signer (not the authorized withdrawer).
        {
            let wrong_signers: HashSet<Pubkey> = vec![Pubkey::new_unique()].into_iter().collect();
            let transaction_context = new_transaction_context(
                vec![
                    (id(), processor_account.clone()),
                    (vote_pubkey, vote_account.clone()),
                    (new_collector, collector_account),
                ],
                vec![
                    InstructionAccount::new(1, false, true),
                    InstructionAccount::new(2, false, true),
                ],
                &rent,
            );
            let instruction_context = transaction_context.get_next_instruction_context().unwrap();
            assert_eq!(
                update_commission_collector(
                    &instruction_context,
                    0,
                    target_version,
                    1,
                    CommissionKind::InflationRewards,
                    &wrong_signers,
                    &rent,
                ),
                Err(InstructionError::MissingRequiredSignature)
            );
            assert_eq!(
                get_commission_collector(&instruction_context, CommissionKind::InflationRewards),
                original_inflation_collector, // Unchanged
            );
            assert_eq!(
                get_commission_collector(&instruction_context, CommissionKind::BlockRevenue),
                original_block_revenue_collector, // Unchanged
            );
        }

        // Should fail - new collector not system program owned.
        {
            let bad_collector = solana_pubkey::new_rand();
            let non_system_owner = solana_pubkey::new_rand();
            let bad_collector_account =
                AccountSharedData::new(collector_lamports, 0, &non_system_owner);
            let transaction_context = new_transaction_context(
                vec![
                    (id(), processor_account.clone()),
                    (vote_pubkey, vote_account.clone()),
                    (bad_collector, bad_collector_account),
                ],
                vec![
                    InstructionAccount::new(1, false, true),
                    InstructionAccount::new(2, false, true),
                ],
                &rent,
            );
            let instruction_context = transaction_context.get_next_instruction_context().unwrap();
            assert_eq!(
                update_commission_collector(
                    &instruction_context,
                    0,
                    target_version,
                    1,
                    CommissionKind::InflationRewards,
                    &signers,
                    &rent,
                ),
                Err(InstructionError::InvalidAccountOwner)
            );
            assert_eq!(
                get_commission_collector(&instruction_context, CommissionKind::InflationRewards),
                original_inflation_collector, // Unchanged
            );
            assert_eq!(
                get_commission_collector(&instruction_context, CommissionKind::BlockRevenue),
                original_block_revenue_collector, // Unchanged
            );
        }

        // Should fail - new collector not rent-exempt.
        {
            let bad_collector = solana_pubkey::new_rand();
            let bad_collector_account = AccountSharedData::new(0, 0, &system_program::id());
            let transaction_context = new_transaction_context(
                vec![
                    (id(), processor_account.clone()),
                    (vote_pubkey, vote_account.clone()),
                    (bad_collector, bad_collector_account),
                ],
                vec![
                    InstructionAccount::new(1, false, true),
                    InstructionAccount::new(2, false, true),
                ],
                &rent,
            );
            let instruction_context = transaction_context.get_next_instruction_context().unwrap();
            assert_eq!(
                update_commission_collector(
                    &instruction_context,
                    0,
                    target_version,
                    1,
                    CommissionKind::InflationRewards,
                    &signers,
                    &rent,
                ),
                Err(InstructionError::InsufficientFunds)
            );
            assert_eq!(
                get_commission_collector(&instruction_context, CommissionKind::InflationRewards),
                original_inflation_collector, // Unchanged
            );
            assert_eq!(
                get_commission_collector(&instruction_context, CommissionKind::BlockRevenue),
                original_block_revenue_collector, // Unchanged
            );
        }

        // Should fail - new collector not writable (reserved account check).
        {
            let bad_collector = solana_pubkey::new_rand();
            let bad_collector_account =
                AccountSharedData::new(collector_lamports, 0, &system_program::id());
            let transaction_context = new_transaction_context(
                vec![
                    (id(), processor_account),
                    (vote_pubkey, vote_account.clone()),
                    (bad_collector, bad_collector_account),
                ],
                vec![
                    InstructionAccount::new(1, false, true),
                    InstructionAccount::new(2, false, false), // not writable
                ],
                &rent,
            );
            let instruction_context = transaction_context.get_next_instruction_context().unwrap();
            assert_eq!(
                update_commission_collector(
                    &instruction_context,
                    0,
                    target_version,
                    1,
                    CommissionKind::InflationRewards,
                    &signers,
                    &rent,
                ),
                Err(InstructionError::InvalidArgument)
            );
            assert_eq!(
                get_commission_collector(&instruction_context, CommissionKind::InflationRewards),
                original_inflation_collector, // Unchanged
            );
            assert_eq!(
                get_commission_collector(&instruction_context, CommissionKind::BlockRevenue),
                original_block_revenue_collector, // Unchanged
            );
        }
    }

    #[test]
    fn test_initialize_account_v2() {
        let target_version = VoteStateTargetVersion::V4;
        let rent = Rent::default();
        let processor_account = AccountSharedData::new(0, 0, &solana_sdk_ids::native_loader::id());

        let vote_pubkey = solana_pubkey::new_rand();
        let node_pubkey = solana_pubkey::new_rand();
        let authorized_voter = solana_pubkey::new_rand();
        let authorized_withdrawer = solana_pubkey::new_rand();
        let inflation_collector_pubkey = solana_pubkey::new_rand();
        let block_revenue_collector_pubkey = solana_pubkey::new_rand();

        let (bls_pubkey, bls_proof_of_possession) =
            create_bls_pubkey_and_proof_of_possession(&vote_pubkey);
        let vote_init = VoteInitV2 {
            node_pubkey,
            authorized_voter,
            authorized_voter_bls_pubkey: bls_pubkey,
            authorized_voter_bls_proof_of_possession: bls_proof_of_possession,
            authorized_withdrawer,
            inflation_rewards_commission_bps: 1_234,
            block_revenue_commission_bps: 5_678,
        };

        let signers: HashSet<Pubkey> = vec![node_pubkey].into_iter().collect();
        let clock = Clock::default();

        let v4_size = VoteStateV4::size_of();
        let lamports = rent.minimum_balance(v4_size);
        let make_uninit_vote_account = || AccountSharedData::new(lamports, v4_size, &id());
        let valid_collector_account =
            || AccountSharedData::new(rent.minimum_balance(0), 0, &system_program::id());

        let assert_v4_fields =
            |instruction_context: &InstructionContext,
             expected_inflation_rewards_collector: Pubkey,
             expected_block_revenue_collector: Pubkey| {
                let vote_account = instruction_context
                    .try_borrow_instruction_account(0)
                    .unwrap();
                let VoteStateVersions::V4(v4) =
                    vote_account.get_state::<VoteStateVersions>().unwrap()
                else {
                    panic!("expected v4");
                };
                assert_eq!(v4.node_pubkey, node_pubkey);
                assert_eq!(
                    v4.authorized_voters.get_authorized_voter(clock.epoch),
                    Some(authorized_voter),
                );
                assert_eq!(v4.authorized_withdrawer, authorized_withdrawer);
                assert_eq!(v4.bls_pubkey_compressed, Some(bls_pubkey));
                assert_eq!(v4.inflation_rewards_commission_bps, 1_234);
                assert_eq!(v4.block_revenue_commission_bps, 5_678);
                assert_eq!(
                    v4.inflation_rewards_collector,
                    expected_inflation_rewards_collector
                );
                assert_eq!(v4.block_revenue_collector, expected_block_revenue_collector);
                assert_eq!(v4.pending_delegator_rewards, 0);
                assert!(v4.votes.is_empty());
                assert_eq!(v4.root_slot, None);
                assert!(v4.epoch_credits.is_empty());
            };

        let assert_still_uninitialized = |instruction_context: &InstructionContext| {
            let vote_account = instruction_context
                .try_borrow_instruction_account(0)
                .unwrap();
            assert!(
                vote_account
                    .get_state::<VoteStateVersions>()
                    .unwrap()
                    .is_uninitialized()
            );
        };

        // Should pass - both collectors are separate accounts.
        {
            let transaction_context = new_transaction_context(
                vec![
                    (id(), processor_account.clone()),
                    (vote_pubkey, make_uninit_vote_account()),
                    (inflation_collector_pubkey, valid_collector_account()),
                    (block_revenue_collector_pubkey, valid_collector_account()),
                ],
                vec![
                    InstructionAccount::new(1, false, true),
                    InstructionAccount::new(2, false, true),
                    InstructionAccount::new(3, false, true),
                ],
                &rent,
            );
            let instruction_context = transaction_context.get_next_instruction_context().unwrap();
            initialize_account_v2(
                &instruction_context,
                0,
                target_version,
                &vote_init,
                1,
                2,
                &signers,
                &clock,
                &rent,
                || Ok(()),
            )
            .unwrap();

            assert_v4_fields(
                &instruction_context,
                inflation_collector_pubkey,
                block_revenue_collector_pubkey,
            );
        }

        // Should pass - inflation collector aliased to vote account.
        {
            let transaction_context = new_transaction_context(
                vec![
                    (id(), processor_account.clone()),
                    (vote_pubkey, make_uninit_vote_account()),
                    (block_revenue_collector_pubkey, valid_collector_account()),
                ],
                vec![
                    InstructionAccount::new(1, false, true),
                    InstructionAccount::new(1, false, true),
                    InstructionAccount::new(2, false, true),
                ],
                &rent,
            );
            let instruction_context = transaction_context.get_next_instruction_context().unwrap();
            initialize_account_v2(
                &instruction_context,
                0,
                target_version,
                &vote_init,
                1,
                2,
                &signers,
                &clock,
                &rent,
                || Ok(()),
            )
            .unwrap();

            assert_v4_fields(
                &instruction_context,
                vote_pubkey,
                block_revenue_collector_pubkey,
            );
        }

        // Should pass - block revenue collector aliased to vote account.
        {
            let transaction_context = new_transaction_context(
                vec![
                    (id(), processor_account.clone()),
                    (vote_pubkey, make_uninit_vote_account()),
                    (inflation_collector_pubkey, valid_collector_account()),
                ],
                vec![
                    InstructionAccount::new(1, false, true),
                    InstructionAccount::new(2, false, true),
                    InstructionAccount::new(1, false, true),
                ],
                &rent,
            );
            let instruction_context = transaction_context.get_next_instruction_context().unwrap();
            initialize_account_v2(
                &instruction_context,
                0,
                target_version,
                &vote_init,
                1,
                2,
                &signers,
                &clock,
                &rent,
                || Ok(()),
            )
            .unwrap();

            assert_v4_fields(
                &instruction_context,
                inflation_collector_pubkey,
                vote_pubkey,
            );
        }

        // Should pass - both collectors aliased to vote account.
        {
            let transaction_context = new_transaction_context(
                vec![
                    (id(), processor_account.clone()),
                    (vote_pubkey, make_uninit_vote_account()),
                ],
                vec![
                    InstructionAccount::new(1, false, true),
                    InstructionAccount::new(1, false, true),
                    InstructionAccount::new(1, false, true),
                ],
                &rent,
            );
            let instruction_context = transaction_context.get_next_instruction_context().unwrap();
            initialize_account_v2(
                &instruction_context,
                0,
                target_version,
                &vote_init,
                1,
                2,
                &signers,
                &clock,
                &rent,
                || Ok(()),
            )
            .unwrap();

            assert_v4_fields(&instruction_context, vote_pubkey, vote_pubkey);
        }

        // Should pass - both collectors aliased to same external account.
        {
            let transaction_context = new_transaction_context(
                vec![
                    (id(), processor_account.clone()),
                    (vote_pubkey, make_uninit_vote_account()),
                    (inflation_collector_pubkey, valid_collector_account()),
                ],
                vec![
                    InstructionAccount::new(1, false, true),
                    InstructionAccount::new(2, false, true),
                    InstructionAccount::new(2, false, true),
                ],
                &rent,
            );
            let instruction_context = transaction_context.get_next_instruction_context().unwrap();
            initialize_account_v2(
                &instruction_context,
                0,
                target_version,
                &vote_init,
                1,
                2,
                &signers,
                &clock,
                &rent,
                || Ok(()),
            )
            .unwrap();

            assert_v4_fields(
                &instruction_context,
                inflation_collector_pubkey,
                inflation_collector_pubkey,
            );
        }

        // Should fail - vote account is the wrong size.
        {
            let oversized_vote_account =
                AccountSharedData::new(rent.minimum_balance(2 * v4_size), 2 * v4_size, &id());
            let transaction_context = new_transaction_context(
                vec![
                    (id(), processor_account.clone()),
                    (vote_pubkey, oversized_vote_account),
                    (inflation_collector_pubkey, valid_collector_account()),
                    (block_revenue_collector_pubkey, valid_collector_account()),
                ],
                vec![
                    InstructionAccount::new(1, false, true),
                    InstructionAccount::new(2, false, true),
                    InstructionAccount::new(3, false, true),
                ],
                &rent,
            );
            let instruction_context = transaction_context.get_next_instruction_context().unwrap();
            assert_eq!(
                initialize_account_v2(
                    &instruction_context,
                    0,
                    target_version,
                    &vote_init,
                    1,
                    2,
                    &signers,
                    &clock,
                    &rent,
                    || Ok(()),
                ),
                Err(InstructionError::InvalidAccountData),
            );
        }

        // Should fail - not a valid vote state.
        {
            let mut invalid_vote_account = AccountSharedData::new(lamports, v4_size, &id());
            invalid_vote_account.set_data_from_slice(&vec![0xFFu8; v4_size]);

            let transaction_context = new_transaction_context(
                vec![
                    (id(), processor_account.clone()),
                    (vote_pubkey, invalid_vote_account),
                    (inflation_collector_pubkey, valid_collector_account()),
                    (block_revenue_collector_pubkey, valid_collector_account()),
                ],
                vec![
                    InstructionAccount::new(1, false, true),
                    InstructionAccount::new(2, false, true),
                    InstructionAccount::new(3, false, true),
                ],
                &rent,
            );
            let instruction_context = transaction_context.get_next_instruction_context().unwrap();
            assert_eq!(
                initialize_account_v2(
                    &instruction_context,
                    0,
                    target_version,
                    &vote_init,
                    1,
                    2,
                    &signers,
                    &clock,
                    &rent,
                    || Ok(()),
                ),
                Err(InstructionError::InvalidAccountData),
            );
        }

        // Should fail - vote account already initialized.
        {
            let preexisting_handler = vote_state_new_for_test(&vote_pubkey, target_version);
            let preexisting_state = preexisting_handler.as_ref_v4().clone();
            let serialized = preexisting_handler.serialize();
            let serialized_len = serialized.len();
            let mut initialized_vote_account =
                AccountSharedData::new(rent.minimum_balance(serialized_len), serialized_len, &id());
            initialized_vote_account.set_data_from_slice(&serialized);

            let transaction_context = new_transaction_context(
                vec![
                    (id(), processor_account.clone()),
                    (vote_pubkey, initialized_vote_account),
                    (inflation_collector_pubkey, valid_collector_account()),
                    (block_revenue_collector_pubkey, valid_collector_account()),
                ],
                vec![
                    InstructionAccount::new(1, false, true),
                    InstructionAccount::new(2, false, true),
                    InstructionAccount::new(3, false, true),
                ],
                &rent,
            );
            let instruction_context = transaction_context.get_next_instruction_context().unwrap();
            assert_eq!(
                initialize_account_v2(
                    &instruction_context,
                    0,
                    target_version,
                    &vote_init,
                    1,
                    2,
                    &signers,
                    &clock,
                    &rent,
                    || Ok(()),
                ),
                Err(InstructionError::AccountAlreadyInitialized),
            );

            // Pre-existing state must be untouched - the new init payload must
            // not have been written.
            let borrowed_vote_account = instruction_context
                .try_borrow_instruction_account(0)
                .unwrap();
            let handler =
                get_vote_state_handler_checked(&borrowed_vote_account, target_version).unwrap();
            assert_eq!(*handler.as_ref_v4(), preexisting_state);
        }

        // Should fail - node_pubkey didn't sign.
        {
            let non_signers: HashSet<Pubkey> = HashSet::new();
            let transaction_context = new_transaction_context(
                vec![
                    (id(), processor_account.clone()),
                    (vote_pubkey, make_uninit_vote_account()),
                    (inflation_collector_pubkey, valid_collector_account()),
                    (block_revenue_collector_pubkey, valid_collector_account()),
                ],
                vec![
                    InstructionAccount::new(1, false, true),
                    InstructionAccount::new(2, false, true),
                    InstructionAccount::new(3, false, true),
                ],
                &rent,
            );
            let instruction_context = transaction_context.get_next_instruction_context().unwrap();
            assert_eq!(
                initialize_account_v2(
                    &instruction_context,
                    0,
                    target_version,
                    &vote_init,
                    1,
                    2,
                    &non_signers,
                    &clock,
                    &rent,
                    || Ok(()),
                ),
                Err(InstructionError::MissingRequiredSignature),
            );
            assert_still_uninitialized(&instruction_context);
        }

        // Should fail - SIMD-0232 collector account checks, applied to both
        // account indices.
        {
            #[derive(Clone, Copy)]
            enum CollectorSlot {
                Inflation,
                BlockRevenue,
            }

            let test_bad_collector =
                |slot: CollectorSlot,
                 bad_collector: AccountSharedData,
                 bad_collector_is_writable: bool,
                 expected_error: InstructionError| {
                    let (
                        inflation_account,
                        inflation_writable,
                        block_revenue_account,
                        block_revenue_writable,
                    ) = match slot {
                        CollectorSlot::Inflation => (
                            bad_collector,
                            bad_collector_is_writable,
                            valid_collector_account(),
                            true,
                        ),
                        CollectorSlot::BlockRevenue => (
                            valid_collector_account(),
                            true,
                            bad_collector,
                            bad_collector_is_writable,
                        ),
                    };

                    let transaction_context = new_transaction_context(
                        vec![
                            (id(), processor_account.clone()),
                            (vote_pubkey, make_uninit_vote_account()),
                            (inflation_collector_pubkey, inflation_account),
                            (block_revenue_collector_pubkey, block_revenue_account),
                        ],
                        vec![
                            InstructionAccount::new(1, false, true),
                            InstructionAccount::new(2, false, inflation_writable),
                            InstructionAccount::new(3, false, block_revenue_writable),
                        ],
                        &rent,
                    );
                    let instruction_context =
                        transaction_context.get_next_instruction_context().unwrap();
                    assert_eq!(
                        initialize_account_v2(
                            &instruction_context,
                            0,
                            target_version,
                            &vote_init,
                            1,
                            2,
                            &signers,
                            &clock,
                            &rent,
                            || Ok(()),
                        ),
                        Err(expected_error),
                    );
                    assert_still_uninitialized(&instruction_context);
                };

            for slot in [CollectorSlot::Inflation, CollectorSlot::BlockRevenue] {
                // 1. Not system-owned.
                test_bad_collector(
                    slot,
                    AccountSharedData::new(rent.minimum_balance(0), 0, &solana_pubkey::new_rand()),
                    true,
                    InstructionError::InvalidAccountOwner,
                );

                // 2. Not rent-exempt.
                test_bad_collector(
                    slot,
                    AccountSharedData::new(0, 0, &system_program::id()),
                    true,
                    InstructionError::InsufficientFunds,
                );

                // 3. Not writable.
                test_bad_collector(
                    slot,
                    valid_collector_account(),
                    false,
                    InstructionError::InvalidArgument,
                );
            }
        }

        // Should fail - BLS proof of possession does not verify.
        {
            let bad_vote_init = VoteInitV2 {
                authorized_voter_bls_pubkey: [1u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE],
                authorized_voter_bls_proof_of_possession: [2u8;
                    BLS_PROOF_OF_POSSESSION_COMPRESSED_SIZE],
                ..vote_init
            };
            let transaction_context = new_transaction_context(
                vec![
                    (id(), processor_account),
                    (vote_pubkey, make_uninit_vote_account()),
                    (inflation_collector_pubkey, valid_collector_account()),
                    (block_revenue_collector_pubkey, valid_collector_account()),
                ],
                vec![
                    InstructionAccount::new(1, false, true),
                    InstructionAccount::new(2, false, true),
                    InstructionAccount::new(3, false, true),
                ],
                &rent,
            );
            let instruction_context = transaction_context.get_next_instruction_context().unwrap();
            assert_eq!(
                initialize_account_v2(
                    &instruction_context,
                    0,
                    target_version,
                    &bad_vote_init,
                    1,
                    2,
                    &signers,
                    &clock,
                    &rent,
                    || Ok(()),
                ),
                Err(InstructionError::InvalidArgument),
            );
            assert_still_uninitialized(&instruction_context);
        }
    }

    /// recipient at index 2.
    fn setup_withdraw_context(
        vote_pubkey: Pubkey,
        vote_account: AccountSharedData,
    ) -> TransactionContext<'static> {
        let rent = Rent::default();
        let recipient = solana_pubkey::new_rand();
        let program_account = AccountSharedData::new(0, 0, &solana_sdk_ids::native_loader::id());
        let mut transaction_context = TransactionContext::new(
            vec![
                (id(), program_account),
                (vote_pubkey, vote_account),
                (recipient, AccountSharedData::default()),
            ],
            rent,
            0,
            0,
            1,
        );
        transaction_context
            .configure_top_level_instruction_for_tests(
                0,
                vec![
                    InstructionAccount::new(1, false, true),
                    InstructionAccount::new(2, false, true),
                ],
                vec![],
            )
            .unwrap();
        transaction_context
    }

    #[test_case(VoteStateTargetVersion::V4 ; "VoteStateV4")]
    fn test_withdraw(target_version: VoteStateTargetVersion) {
        // Verify withdraw boundary conditions around the rent-exempt
        // minimum: partial withdraw, full deinit, and over-withdraw.
        let vote_pubkey = solana_pubkey::new_rand();
        let vote_state = vote_state_new_for_test(&vote_pubkey, target_version);
        let withdrawer = *vote_state.authorized_withdrawer();
        let signers: HashSet<Pubkey> = [withdrawer].into_iter().collect();
        let rent = Rent::default();
        let serialized = vote_state.clone().serialize();
        let serialized_len = serialized.len();
        let min_balance = rent.minimum_balance(serialized_len);
        let clock = Clock {
            epoch: 100,
            ..Clock::default()
        };

        // Account at exact rent-exempt minimum: withdraw 1 fails.
        {
            let mut acct = AccountSharedData::new(min_balance, serialized_len, &id());
            acct.set_data_from_slice(&serialized);
            let transaction_context = setup_withdraw_context(vote_pubkey, acct);
            let ix = transaction_context.get_next_instruction_context().unwrap();
            assert_eq!(
                withdraw(&ix, 0, target_version, 1, 1, &signers, &rent, &clock),
                Err(InstructionError::InsufficientFunds)
            );
        }

        // Account at exact rent-exempt minimum: withdraw all succeeds (deinit).
        {
            let mut acct = AccountSharedData::new(min_balance, serialized_len, &id());
            acct.set_data_from_slice(&serialized);
            let transaction_context = setup_withdraw_context(vote_pubkey, acct);
            let ix = transaction_context.get_next_instruction_context().unwrap();
            withdraw(
                &ix,
                0,
                target_version,
                min_balance,
                1,
                &signers,
                &rent,
                &clock,
            )
            .unwrap();
        }

        // Account at rent_exempt + 100: withdraw 100 succeeds.
        {
            let mut acct = AccountSharedData::new(min_balance + 100, serialized_len, &id());
            acct.set_data_from_slice(&serialized);
            let transaction_context = setup_withdraw_context(vote_pubkey, acct);
            let ix = transaction_context.get_next_instruction_context().unwrap();
            withdraw(&ix, 0, target_version, 100, 1, &signers, &rent, &clock).unwrap();
        }

        // Account at rent_exempt + 100: withdraw 101 fails.
        {
            let mut acct = AccountSharedData::new(min_balance + 100, serialized_len, &id());
            acct.set_data_from_slice(&serialized);
            let transaction_context = setup_withdraw_context(vote_pubkey, acct);
            let ix = transaction_context.get_next_instruction_context().unwrap();
            assert_eq!(
                withdraw(&ix, 0, target_version, 101, 1, &signers, &rent, &clock),
                Err(InstructionError::InsufficientFunds)
            );
        }
    }

    /// Helper to create a V4 vote account with a specific
    /// `pending_delegator_rewards` value.
    fn make_v4_account_with_pending(
        vote_pubkey: &Pubkey,
        pending: u64,
        extra_lamports: u64,
    ) -> (VoteStateHandler, AccountSharedData) {
        let vote_state = vote_state_new_for_test(vote_pubkey, VoteStateTargetVersion::V4);
        let mut v4 = vote_state.as_ref_v4().clone();
        v4.pending_delegator_rewards = pending;
        let handler = VoteStateHandler::new_v4(v4);
        let serialized = handler.clone().serialize();
        let rent = Rent::default();
        let lamports = rent.minimum_balance(serialized.len()) + extra_lamports;
        let mut account = AccountSharedData::new(lamports, serialized.len(), &id());
        account.set_data_from_slice(&serialized);
        (handler, account)
    }

    #[test]
    fn test_withdraw_with_pending_delegator_rewards() {
        // Verify withdraw protects pending_delegator_rewards: partial
        // withdrawals respect the pending reserve, and full close is
        // blocked when pending > 0.
        let vote_pubkey = solana_pubkey::new_rand();
        let rent = Rent::default();
        let clock = Clock {
            epoch: 100,
            ..Clock::default()
        };

        // pending = 1000, extra = 1000. withdrawable = 0.
        {
            let (handler, account) = make_v4_account_with_pending(&vote_pubkey, 1000, 1000);
            let withdrawer = *handler.authorized_withdrawer();
            let signers: HashSet<Pubkey> = [withdrawer].into_iter().collect();
            let tx = setup_withdraw_context(vote_pubkey, account);
            let ix = tx.get_next_instruction_context().unwrap();

            // Withdraw 1 fails (withdrawable = lamports - rent - pending = 0).
            assert_eq!(
                withdraw(
                    &ix,
                    0,
                    VoteStateTargetVersion::V4,
                    1,
                    1,
                    &signers,
                    &rent,
                    &clock
                ),
                Err(InstructionError::InsufficientFunds)
            );
        }

        // pending = 1000, extra = 1001. withdrawable = 1.
        {
            let (handler, account) = make_v4_account_with_pending(&vote_pubkey, 1000, 1001);
            let withdrawer = *handler.authorized_withdrawer();
            let signers: HashSet<Pubkey> = [withdrawer].into_iter().collect();
            let tx = setup_withdraw_context(vote_pubkey, account);
            let ix = tx.get_next_instruction_context().unwrap();

            // Withdraw 1 succeeds.
            withdraw(
                &ix,
                0,
                VoteStateTargetVersion::V4,
                1,
                1,
                &signers,
                &rent,
                &clock,
            )
            .unwrap();
        }

        // pending = 1000, extra = 1001. Withdraw 2 fails.
        {
            let (handler, account) = make_v4_account_with_pending(&vote_pubkey, 1000, 1001);
            let withdrawer = *handler.authorized_withdrawer();
            let signers: HashSet<Pubkey> = [withdrawer].into_iter().collect();
            let tx = setup_withdraw_context(vote_pubkey, account);
            let ix = tx.get_next_instruction_context().unwrap();

            assert_eq!(
                withdraw(
                    &ix,
                    0,
                    VoteStateTargetVersion::V4,
                    2,
                    1,
                    &signers,
                    &rent,
                    &clock
                ),
                Err(InstructionError::InsufficientFunds)
            );
        }

        // Full close blocked when pending > 0.
        {
            let (handler, account) = make_v4_account_with_pending(&vote_pubkey, 1, 1_000_000);
            let withdrawer = *handler.authorized_withdrawer();
            let signers: HashSet<Pubkey> = [withdrawer].into_iter().collect();
            let lamports = rent.minimum_balance(VoteStateV4::size_of()) + 1_000_000;
            let tx = setup_withdraw_context(vote_pubkey, account);
            let ix = tx.get_next_instruction_context().unwrap();

            assert_eq!(
                withdraw(
                    &ix,
                    0,
                    VoteStateTargetVersion::V4,
                    lamports,
                    1,
                    &signers,
                    &rent,
                    &clock
                ),
                Err(InstructionError::InsufficientFunds)
            );
        }

        // Full close succeeds when pending = 0.
        {
            let (handler, account) = make_v4_account_with_pending(&vote_pubkey, 0, 100);
            let withdrawer = *handler.authorized_withdrawer();
            let signers: HashSet<Pubkey> = [withdrawer].into_iter().collect();
            let lamports = rent.minimum_balance(VoteStateV4::size_of()) + 100;
            let tx = setup_withdraw_context(vote_pubkey, account);
            let ix = tx.get_next_instruction_context().unwrap();

            withdraw(
                &ix,
                0,
                VoteStateTargetVersion::V4,
                lamports,
                1,
                &signers,
                &rent,
                &clock,
            )
            .unwrap();
        }
    }

    /// Build a maximum-size V3 vote state with all variable-length
    /// collections at capacity (votes, epoch_credits, authorized_voters).
    fn get_max_sized_vote_state_v3() -> VoteStateV3 {
        let root_slot = 42u64;
        let votes: VecDeque<LandedVote> = (0..MAX_LOCKOUT_HISTORY)
            .map(|i| LandedVote {
                latency: i as u8,
                lockout: Lockout::new_with_confirmation_count(
                    root_slot + i as u64 + 1,
                    (MAX_LOCKOUT_HISTORY - i) as u32,
                ),
            })
            .collect();
        let epoch_credits: Vec<(u64, u64, u64)> = (0..MAX_EPOCH_CREDITS_HISTORY)
            .map(|i| (i as u64, (i as u64 + 1) * 100, i as u64 * 100))
            .collect();
        let mut authorized_voters = AuthorizedVoters::default();
        for i in 0..=solana_epoch_schedule::MAX_LEADER_SCHEDULE_EPOCH_OFFSET {
            authorized_voters.insert(i, solana_pubkey::new_rand());
        }

        VoteStateV3 {
            node_pubkey: solana_pubkey::new_rand(),
            authorized_withdrawer: solana_pubkey::new_rand(),
            commission: 42,
            votes,
            root_slot: Some(root_slot),
            epoch_credits,
            authorized_voters,
            ..Default::default()
        }
    }

    #[test]
    fn test_v3_to_v4_stale_trailing_bytes() {
        // V4 deserializer must ignore trailing bytes left over from a
        // V3 to V4 conversion in a fixed-size account buffer.
        //
        // The conversion and serialization is driven through the handler, ie.
        // `get_vote_state_handler_checked`/`try_convert_to_vote_state_v4`.
        //
        // We conduct this test through `get_vote_state_handler_checked` to
        // ensure we're testing program code.
        let vote_pubkey = solana_pubkey::new_rand();
        let v3 = get_max_sized_vote_state_v3();
        let node_pubkey = v3.node_pubkey;
        let authorized_withdrawer = v3.authorized_withdrawer;
        let commission = v3.commission;
        let root_slot = v3.root_slot;
        let votes = v3.votes.clone();
        let epoch_credits = v3.epoch_credits.clone();
        let authorized_voters = v3.authorized_voters.clone();
        let last_timestamp = v3.last_timestamp.clone();

        // Serialize V3 into a fixed-size account buffer.
        let buf_size = VoteStateV3::size_of();
        let v3_versioned = VoteStateVersions::V3(Box::new(v3));
        let v3_serialized_len = bincode::serialized_size(&v3_versioned).unwrap() as usize;
        let mut vote_account_data = vec![0u8; buf_size];
        bincode::serialize_into(&mut vote_account_data[..], &v3_versioned).unwrap();

        // Drive V3 to V4 conversion through the program handler.
        let rent = Rent::default();
        let lamports = rent.minimum_balance(buf_size) + 1_000_000;
        let mut vote_account = AccountSharedData::new(lamports, buf_size, &id());
        vote_account.set_data_from_slice(&vote_account_data);
        let program_account = AccountSharedData::new(0, 0, &solana_sdk_ids::native_loader::id());
        let transaction_context = new_transaction_context(
            vec![(id(), program_account), (vote_pubkey, vote_account)],
            vec![InstructionAccount::new(1, false, true)],
            &rent,
        );
        let ix = transaction_context.get_next_instruction_context().unwrap();
        let mut borrowed = ix.try_borrow_instruction_account(0).unwrap();

        // `get_vote_state_handler_checked` with V4 target triggers the full
        // deser -> conversion path; `set_vote_account_state` writes it back.
        let vote_state =
            get_vote_state_handler_checked(&borrowed, VoteStateTargetVersion::V4).unwrap();
        vote_state.set_vote_account_state(&mut borrowed).unwrap();

        // Inspect raw account data written by the handler.
        let account_data = borrowed.get_data();
        let v4_serialized_len = {
            let v4 = VoteStateV4::deserialize(account_data, &vote_pubkey).unwrap();
            bincode::serialized_size(&VoteStateVersions::new_v4(v4)).unwrap() as usize
        };
        assert!(
            v4_serialized_len < v3_serialized_len,
            "v4 ({v4_serialized_len}) should be smaller than v3 ({v3_serialized_len})",
        );

        // The V4 deserializer must produce the correct state despite
        // trailing bytes left from the larger V3 serialization.
        let deserialized = VoteStateV4::deserialize(account_data, &vote_pubkey).unwrap();
        assert_eq!(deserialized.node_pubkey, node_pubkey);
        assert_eq!(deserialized.authorized_withdrawer, authorized_withdrawer);
        assert_eq!(deserialized.root_slot, root_slot);
        assert_eq!(deserialized.votes, votes);
        assert_eq!(deserialized.epoch_credits, epoch_credits);
        assert_eq!(deserialized.authorized_voters, authorized_voters);
        assert_eq!(
            deserialized.inflation_rewards_commission_bps,
            commission as u16 * 100
        );
        assert_eq!(deserialized.last_timestamp, last_timestamp);

        // Fill the trailing region with non-zero garbage, then round-trip
        // through the handler again to verify the program handles it.
        borrowed.get_data_mut().unwrap()[v4_serialized_len..].fill(0xDE);

        let vote_state =
            get_vote_state_handler_checked(&borrowed, VoteStateTargetVersion::V4).unwrap();
        vote_state.set_vote_account_state(&mut borrowed).unwrap();

        let deserialized = VoteStateV4::deserialize(borrowed.get_data(), &vote_pubkey).unwrap();
        assert_eq!(deserialized.node_pubkey, node_pubkey);
        assert_eq!(deserialized.authorized_withdrawer, authorized_withdrawer);
        assert_eq!(deserialized.root_slot, root_slot);
        assert_eq!(deserialized.votes, votes);
        assert_eq!(deserialized.epoch_credits, epoch_credits);
        assert_eq!(deserialized.authorized_voters, authorized_voters);
        assert_eq!(
            deserialized.inflation_rewards_commission_bps,
            commission as u16 * 100
        );
        assert_eq!(deserialized.last_timestamp, last_timestamp);
    }

    #[test]
    fn test_v3_to_v4_trailing_bytes_shrink_and_regrow() {
        // Exercises the full lifecycle of trailing-byte behavior in a
        // fixed-size account buffer, driven through the program handlers
        // `get_vote_state_handler_checked` and `set_vote_account_state`:
        // * Step 1: Start with a max-size V3 state (all collections full).
        // * Step 2: Convert V3 -> V4 via the handler. V4's serialized form
        //           is smaller, leaving trailing garbage.
        // * Step 3: Clear all votes, simulating the extreme case of a
        //           validator whose entire tower has expired, which shrinks
        //           the v4 vote state.
        // * Step 4: Re-add votes incrementally, round-tripping through the
        //           handler each time to verify the program handles the
        //           growing serialized region over stale trailing bytes.

        let vote_pubkey = solana_pubkey::new_rand();
        let v3 = get_max_sized_vote_state_v3();
        let node_pubkey = v3.node_pubkey;
        let authorized_withdrawer = v3.authorized_withdrawer;
        let commission = v3.commission;
        let root_slot = v3.root_slot;
        let votes = v3.votes.clone();
        let epoch_credits = v3.epoch_credits.clone();
        let authorized_voters = v3.authorized_voters.clone();
        let last_timestamp = v3.last_timestamp.clone();

        // Step 1: Populate V3 account and record V3 serialized size.
        let buf_size = VoteStateV3::size_of();
        let v3_versioned = VoteStateVersions::V3(Box::new(v3));
        let v3_serialized_len = bincode::serialized_size(&v3_versioned).unwrap() as usize;
        let mut vote_account_data = vec![0u8; buf_size];
        bincode::serialize_into(&mut vote_account_data[..], &v3_versioned).unwrap();

        let rent = Rent::default();
        let lamports = rent.minimum_balance(buf_size) + 1_000_000;
        let mut vote_account = AccountSharedData::new(lamports, buf_size, &id());
        vote_account.set_data_from_slice(&vote_account_data);
        let program_account = AccountSharedData::new(0, 0, &solana_sdk_ids::native_loader::id());
        let transaction_context = new_transaction_context(
            vec![(id(), program_account), (vote_pubkey, vote_account)],
            vec![InstructionAccount::new(1, false, true)],
            &rent,
        );
        let ix = transaction_context.get_next_instruction_context().unwrap();
        let mut borrowed = ix.try_borrow_instruction_account(0).unwrap();

        // Step 2: V3 -> V4 conversion via the handler.
        let vote_state =
            get_vote_state_handler_checked(&borrowed, VoteStateTargetVersion::V4).unwrap();
        vote_state.set_vote_account_state(&mut borrowed).unwrap();

        let v4_after_convert = VoteStateV4::deserialize(borrowed.get_data(), &vote_pubkey).unwrap();
        let v4_serialized_len =
            bincode::serialized_size(&VoteStateVersions::new_v4(v4_after_convert.clone())).unwrap()
                as usize;

        assert!(
            v4_serialized_len < v3_serialized_len,
            "v4 ({v4_serialized_len}) should be smaller than v3 ({v3_serialized_len})",
        );
        let trailing_len_after_convert = buf_size - v4_serialized_len;
        assert!(
            trailing_len_after_convert > 0,
            "expected trailing bytes after v3 -> v4 conversion"
        );

        // Verify field-level correctness of the converted state.
        assert_eq!(v4_after_convert.node_pubkey, node_pubkey);
        assert_eq!(
            v4_after_convert.authorized_withdrawer,
            authorized_withdrawer
        );
        assert_eq!(v4_after_convert.root_slot, root_slot);
        assert_eq!(v4_after_convert.votes, votes);
        assert_eq!(v4_after_convert.epoch_credits, epoch_credits);
        assert_eq!(v4_after_convert.authorized_voters, authorized_voters);
        assert_eq!(
            v4_after_convert.inflation_rewards_commission_bps,
            commission as u16 * 100,
        );
        assert_eq!(v4_after_convert.last_timestamp, last_timestamp);

        // Step 3a: Clear all votes, round-trip with resulting stale bytes.
        let mut v4_empty_votes = v4_after_convert.clone();
        v4_empty_votes.votes.clear();
        let v4_empty_serialized_len =
            bincode::serialized_size(&VoteStateVersions::new_v4(v4_empty_votes.clone())).unwrap()
                as usize;
        assert!(
            v4_empty_serialized_len < v4_serialized_len,
            "empty-votes v4 ({v4_empty_serialized_len}) should be smaller than full v4 \
             ({v4_serialized_len})",
        );

        // Write the vote-cleared state. The trailing region now contains
        // stale bytes from the previous (larger) V4 serialization.
        borrowed
            .set_state(&VoteStateVersions::new_v4(v4_empty_votes.clone()))
            .unwrap();
        let trailing_len_after_clear = buf_size - v4_empty_serialized_len;
        assert!(
            trailing_len_after_clear > trailing_len_after_convert,
            "trailing region should grow after clearing votes: {trailing_len_after_clear} vs \
             {trailing_len_after_convert}",
        );

        // Round-trip through the handler with the stale bytes.
        let vote_state =
            get_vote_state_handler_checked(&borrowed, VoteStateTargetVersion::V4).unwrap();
        vote_state.set_vote_account_state(&mut borrowed).unwrap();

        let deserialized = VoteStateV4::deserialize(borrowed.get_data(), &vote_pubkey).unwrap();
        assert!(deserialized.votes.is_empty(),);
        assert_eq!(deserialized.epoch_credits.len(), MAX_EPOCH_CREDITS_HISTORY,);
        assert_eq!(deserialized.authorized_voters, authorized_voters,);
        assert_eq!(deserialized.last_timestamp, last_timestamp);

        // Step 3b: Fill trailing with explicit garbage and round-trip again.
        //
        // Overwrite the trailing region with a non-zero pattern to verify
        // the handler is not sensitive to arbitrary trailing content.
        borrowed.get_data_mut().unwrap()[v4_empty_serialized_len..].fill(0xCD);

        let vote_state =
            get_vote_state_handler_checked(&borrowed, VoteStateTargetVersion::V4).unwrap();
        vote_state.set_vote_account_state(&mut borrowed).unwrap();

        let deserialized = VoteStateV4::deserialize(borrowed.get_data(), &vote_pubkey).unwrap();
        assert!(deserialized.votes.is_empty());
        assert_eq!(deserialized.epoch_credits.len(), MAX_EPOCH_CREDITS_HISTORY);
        assert_eq!(deserialized.authorized_voters, authorized_voters);
        assert_eq!(deserialized.last_timestamp, last_timestamp);

        // Step 4: Re-add votes, growing the serialized region.
        //
        // Incrementally add votes back, writing each state and filling
        // trailing with garbage, then round-tripping through the handler
        // to verify it handles the growing data region correctly.
        let mut v4_regrowing = v4_empty_votes;
        for i in 0..MAX_LOCKOUT_HISTORY {
            v4_regrowing.votes.push_back(LandedVote {
                latency: (i % 256) as u8,
                lockout: Lockout::new_with_confirmation_count(
                    root_slot.unwrap() + 1000 + i as u64,
                    (MAX_LOCKOUT_HISTORY - i) as u32,
                ),
            });

            // Write the updated state, fill trailing with garbage.
            borrowed
                .set_state(&VoteStateVersions::new_v4(v4_regrowing.clone()))
                .unwrap();
            let current_serialized_len =
                bincode::serialized_size(&VoteStateVersions::new_v4(v4_regrowing.clone())).unwrap()
                    as usize;
            let current_trailing = buf_size - current_serialized_len;
            assert!(
                current_trailing < trailing_len_after_clear,
                "trailing region should shrink as votes are added"
            );
            if current_serialized_len < buf_size {
                borrowed.get_data_mut().unwrap()[current_serialized_len..].fill(0xEF);
            }

            // Round-trip through the handler to verify.
            let vote_state =
                get_vote_state_handler_checked(&borrowed, VoteStateTargetVersion::V4).unwrap();
            vote_state.set_vote_account_state(&mut borrowed).unwrap();

            let deserialized = VoteStateV4::deserialize(borrowed.get_data(), &vote_pubkey).unwrap();
            assert_eq!(
                deserialized.votes.len(),
                i + 1,
                "expected {} votes after re-adding",
                i + 1,
            );
            assert_eq!(deserialized, v4_regrowing);
        }

        // Final consistency check: all votes are back and all fields correct.
        let final_deserialized =
            VoteStateV4::deserialize(borrowed.get_data(), &vote_pubkey).unwrap();
        assert_eq!(final_deserialized.votes.len(), MAX_LOCKOUT_HISTORY);
        assert_eq!(final_deserialized.epoch_credits, epoch_credits);
        assert_eq!(final_deserialized.authorized_voters, authorized_voters);
        assert_eq!(final_deserialized.last_timestamp, last_timestamp);
    }

    #[test]
    fn test_bls_absent_after_v3_to_v4_migration() {
        // V3 to V4 migration via get_vote_state_handler_checked must
        // produce bls_pubkey_compressed = None.
        let vote_pubkey = solana_pubkey::new_rand();
        let v3 = VoteStateV3::new(
            &VoteInit {
                node_pubkey: solana_pubkey::new_rand(),
                authorized_voter: solana_pubkey::new_rand(),
                authorized_withdrawer: solana_pubkey::new_rand(),
                commission: 10,
            },
            &Clock::default(),
        );
        let last_timestamp = v3.last_timestamp.clone();

        // Serialize V3 into an account.
        let buf_size = VoteStateV3::size_of();
        let v3_versioned = VoteStateVersions::V3(Box::new(v3));
        let mut vote_account_data = vec![0u8; buf_size];
        bincode::serialize_into(&mut vote_account_data[..], &v3_versioned).unwrap();

        let rent = Rent::default();
        let lamports = rent.minimum_balance(buf_size) + 1_000_000;
        let mut vote_account = AccountSharedData::new(lamports, buf_size, &id());
        vote_account.set_data_from_slice(&vote_account_data);
        let program_account = AccountSharedData::new(0, 0, &solana_sdk_ids::native_loader::id());
        let transaction_context = new_transaction_context(
            vec![(id(), program_account), (vote_pubkey, vote_account)],
            vec![InstructionAccount::new(1, false, true)],
            &rent,
        );
        let ix = transaction_context.get_next_instruction_context().unwrap();
        let mut borrowed = ix.try_borrow_instruction_account(0).unwrap();

        // Drive conversion through the handler.
        let vote_state =
            get_vote_state_handler_checked(&borrowed, VoteStateTargetVersion::V4).unwrap();
        vote_state.set_vote_account_state(&mut borrowed).unwrap();

        let v4 = VoteStateHandler::new_v4(
            VoteStateV4::deserialize(borrowed.get_data(), &vote_pubkey).unwrap(),
        );
        assert_eq!(v4.as_ref_v4().bls_pubkey_compressed, None);
        assert!(!v4.has_bls_pubkey());
        assert_eq!(v4.as_ref_v4().last_timestamp, last_timestamp);
    }

    #[test]
    fn test_bls_overwrite_via_voter_with_bls() {
        // BLS pubkey set to A, then overwritten to B via VoterWithBLS.
        let vote_pubkey = Pubkey::new_unique();
        let (bls_a, pop_a) = create_bls_pubkey_and_proof_of_possession(&vote_pubkey);
        let (bls_b, pop_b) = create_bls_pubkey_and_proof_of_possession(&vote_pubkey);

        let vote_state = vote_state_new_for_test(&vote_pubkey, VoteStateTargetVersion::V4);
        let withdrawer = *vote_state.authorized_withdrawer();
        let node_pubkey = *vote_state.node_pubkey();
        let serialized = vote_state.serialize();
        let rent = Rent::default();
        let lamports = rent.minimum_balance(serialized.len());
        let mut vote_account = AccountSharedData::new(lamports, serialized.len(), &id());
        vote_account.set_data_from_slice(&serialized);
        let program_account = AccountSharedData::new(0, 0, &solana_sdk_ids::native_loader::id());
        let transaction_context = new_transaction_context(
            vec![(id(), program_account), (vote_pubkey, vote_account)],
            vec![InstructionAccount::new(1, false, true)],
            &rent,
        );
        let ix = transaction_context.get_next_instruction_context().unwrap();
        let mut borrowed = ix.try_borrow_instruction_account(0).unwrap();

        let signers: HashSet<Pubkey> = [withdrawer, Pubkey::new_unique()].into_iter().collect();

        // Set BLS A.
        authorize(
            &mut borrowed,
            VoteStateTargetVersion::V4,
            &Pubkey::new_unique(),
            VoteAuthorize::VoterWithBLS(VoterWithBLSArgs {
                bls_pubkey: bls_a,
                bls_proof_of_possession: pop_a,
            }),
            &signers,
            &Clock::default(),
            true,
            || Ok(()),
        )
        .unwrap();

        let v4 = VoteStateV4::deserialize(borrowed.get_data(), &node_pubkey).unwrap();
        assert_eq!(v4.bls_pubkey_compressed, Some(bls_a));

        // Overwrite with BLS B.
        let clock = Clock {
            epoch: 3,
            ..Clock::default()
        };
        authorize(
            &mut borrowed,
            VoteStateTargetVersion::V4,
            &Pubkey::new_unique(),
            VoteAuthorize::VoterWithBLS(VoterWithBLSArgs {
                bls_pubkey: bls_b,
                bls_proof_of_possession: pop_b,
            }),
            &signers,
            &clock,
            true,
            || Ok(()),
        )
        .unwrap();

        let v4 = VoteStateV4::deserialize(borrowed.get_data(), &node_pubkey).unwrap();
        assert_eq!(v4.bls_pubkey_compressed, Some(bls_b));
    }

    #[test]
    fn test_bls_pop_cryptographic_failures() {
        // Invalid PoP scenarios: zero bytes, garbage bytes, and
        // PoP bound to the wrong vote account.
        let vote_pubkey = Pubkey::new_unique();
        let vote_state = vote_state_new_for_test(&vote_pubkey, VoteStateTargetVersion::V4);
        let withdrawer = *vote_state.authorized_withdrawer();
        let serialized = vote_state.serialize();
        let rent = Rent::default();
        let lamports = rent.minimum_balance(serialized.len());
        let mut vote_account = AccountSharedData::new(lamports, serialized.len(), &id());
        vote_account.set_data_from_slice(&serialized);
        let program_account = AccountSharedData::new(0, 0, &solana_sdk_ids::native_loader::id());
        let transaction_context = new_transaction_context(
            vec![(id(), program_account), (vote_pubkey, vote_account)],
            vec![InstructionAccount::new(1, false, true)],
            &rent,
        );
        let ix = transaction_context.get_next_instruction_context().unwrap();
        let mut borrowed = ix.try_borrow_instruction_account(0).unwrap();
        let signers: HashSet<Pubkey> = [withdrawer, Pubkey::new_unique()].into_iter().collect();
        let clock = Clock::default();

        // All-zero BLS pubkey + PoP.
        assert_eq!(
            authorize(
                &mut borrowed,
                VoteStateTargetVersion::V4,
                &Pubkey::new_unique(),
                VoteAuthorize::VoterWithBLS(VoterWithBLSArgs {
                    bls_pubkey: [0u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE],
                    bls_proof_of_possession: [0u8; BLS_PROOF_OF_POSSESSION_COMPRESSED_SIZE],
                }),
                &signers,
                &clock,
                true,
                || Ok(()),
            ),
            Err(InstructionError::InvalidArgument)
        );

        // Random garbage BLS pubkey + PoP.
        assert_eq!(
            authorize(
                &mut borrowed,
                VoteStateTargetVersion::V4,
                &Pubkey::new_unique(),
                VoteAuthorize::VoterWithBLS(VoterWithBLSArgs {
                    bls_pubkey: [0xAB; BLS_PUBLIC_KEY_COMPRESSED_SIZE],
                    bls_proof_of_possession: [0xCD; BLS_PROOF_OF_POSSESSION_COMPRESSED_SIZE],
                }),
                &signers,
                &clock,
                true,
                || Ok(()),
            ),
            Err(InstructionError::InvalidArgument)
        );

        // Valid BLS pubkey but PoP for wrong vote account.
        let other_vote = Pubkey::new_unique();
        let (bls_for_other, pop_for_other) = create_bls_pubkey_and_proof_of_possession(&other_vote);
        assert_eq!(
            authorize(
                &mut borrowed,
                VoteStateTargetVersion::V4,
                &Pubkey::new_unique(),
                VoteAuthorize::VoterWithBLS(VoterWithBLSArgs {
                    bls_pubkey: bls_for_other,
                    bls_proof_of_possession: pop_for_other,
                }),
                &signers,
                &clock,
                true,
                || Ok(()),
            ),
            Err(InstructionError::InvalidArgument)
        );
    }

    #[test]
    fn test_collector_fields_immutable_in_v4_only_scope() {
        // Verify `inflation_rewards_collector` and `block_revenue_collector`
        // are not modified by `Authorize` or `UpdateCommission` post-v4.
        let vote_pubkey = solana_pubkey::new_rand();
        let vote_state = vote_state_new_for_test(&vote_pubkey, VoteStateTargetVersion::V4);
        let withdrawer = *vote_state.authorized_withdrawer();
        let original_inflation_collector = vote_state.as_ref_v4().inflation_rewards_collector;
        let original_block_revenue_collector = vote_state.as_ref_v4().block_revenue_collector;

        let serialized = vote_state.clone().serialize();
        let rent = Rent::default();
        let lamports = rent.minimum_balance(serialized.len());
        let mut vote_account = AccountSharedData::new(lamports, serialized.len(), &id());
        vote_account.set_data_from_slice(&serialized);
        let program_account = AccountSharedData::new(0, 0, &solana_sdk_ids::native_loader::id());
        let transaction_context = new_transaction_context(
            vec![(id(), program_account), (vote_pubkey, vote_account)],
            vec![InstructionAccount::new(1, false, true)],
            &rent,
        );
        let instruction_context = transaction_context.get_next_instruction_context().unwrap();
        let mut borrowed = instruction_context
            .try_borrow_instruction_account(0)
            .unwrap();

        let signers: HashSet<Pubkey> = [withdrawer].into_iter().collect();
        let clock = Clock::default();

        // Authorize: should not change collectors.
        authorize(
            &mut borrowed,
            VoteStateTargetVersion::V4,
            &solana_pubkey::new_rand(),
            VoteAuthorize::Voter,
            &signers,
            &clock,
            false,
            || Ok(()),
        )
        .unwrap();

        let v4 = VoteStateV4::deserialize(borrowed.get_data(), &vote_state.as_ref_v4().node_pubkey)
            .unwrap();
        assert_eq!(v4.inflation_rewards_collector, original_inflation_collector);
        assert_eq!(v4.block_revenue_collector, original_block_revenue_collector);

        // UpdateCommission: should not change collectors.
        update_commission(
            &mut borrowed,
            VoteStateTargetVersion::V4,
            50,
            &signers,
            &solana_epoch_schedule::EpochSchedule::without_warmup(),
            &clock,
            false,
        )
        .unwrap();

        let v4 = VoteStateV4::deserialize(borrowed.get_data(), &vote_state.as_ref_v4().node_pubkey)
            .unwrap();
        assert_eq!(v4.inflation_rewards_collector, original_inflation_collector);
        assert_eq!(v4.block_revenue_collector, original_block_revenue_collector);
    }

    #[test]
    fn test_pending_delegator_rewards_zero_in_v4_only_scope() {
        // Verify `pending_delegator_rewards` stays 0 through post-v4
        // instructions (SIMD-0123 not active).
        let vote_pubkey = solana_pubkey::new_rand();
        let vote_state = vote_state_new_for_test(&vote_pubkey, VoteStateTargetVersion::V4);
        let withdrawer = *vote_state.authorized_withdrawer();

        let serialized = vote_state.clone().serialize();
        let rent = Rent::default();
        let lamports = rent.minimum_balance(serialized.len());
        let mut vote_account = AccountSharedData::new(lamports, serialized.len(), &id());
        vote_account.set_data_from_slice(&serialized);
        let program_account = AccountSharedData::new(0, 0, &solana_sdk_ids::native_loader::id());
        let transaction_context = new_transaction_context(
            vec![(id(), program_account), (vote_pubkey, vote_account)],
            vec![InstructionAccount::new(1, false, true)],
            &rent,
        );
        let instruction_context = transaction_context.get_next_instruction_context().unwrap();
        let mut borrowed = instruction_context
            .try_borrow_instruction_account(0)
            .unwrap();

        assert_eq!(
            VoteStateV4::deserialize(borrowed.get_data(), &vote_state.as_ref_v4().node_pubkey)
                .unwrap()
                .pending_delegator_rewards,
            0
        );

        // Authorize: pending should stay 0.
        let signers: HashSet<Pubkey> = [withdrawer].into_iter().collect();
        authorize(
            &mut borrowed,
            VoteStateTargetVersion::V4,
            &solana_pubkey::new_rand(),
            VoteAuthorize::Voter,
            &signers,
            &Clock::default(),
            false,
            || Ok(()),
        )
        .unwrap();

        assert_eq!(
            VoteStateV4::deserialize(borrowed.get_data(), &vote_state.as_ref_v4().node_pubkey)
                .unwrap()
                .pending_delegator_rewards,
            0
        );
    }
}
