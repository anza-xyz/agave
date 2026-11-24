#![allow(clippy::arithmetic_side_effects)]
use {
    super::*,
    crate::vote_state::{VoteInit, VoteStateV3, VoteStateV4, VoteStateVersions},
    solana_account::{
        AccountSharedData, ReadableAccount, WritableAccount, state_traits::StateMutWincode as _,
    },
    solana_clock::Clock,
    solana_instruction::AccountMeta,
    solana_program_runtime::{
        invoke_context::mock_process_instruction_with_feature_set,
        solana_sbpf::program::BuiltinFunctionDefinition,
    },
    solana_svm_feature_set::SVMFeatureSet,
    solana_vote::location::update_location,
    solana_vote_interface::{
        authorized_voters::AuthorizedVoters,
        state::vote_state_v5::{Location, VOTE_STATE_V5_SIZE, VoteStateV5},
    },
};

const FRANKFURT: (i32, i32, i32) = (4_051_542, 618_526, 4_870_645);
const TOKYO: (i32, i32, i32) = (-3_955_214, 3_355_452, 3_699_409);
const EPOCH: u64 = 10;

fn v4(voter: Pubkey) -> VoteStateV4 {
    let mut state = VoteStateV4::new_with_defaults(
        &Pubkey::new_unique(),
        &VoteInit {
            authorized_voter: voter,
            ..VoteInit::default()
        },
        &Clock::default(),
    );
    state.bls_pubkey_compressed = Some([1; 48]);
    state.authorized_voters.insert(1, voter);
    state.authorized_voters.insert(9, voter);
    state
}

fn account(state: VoteStateV4, len: usize) -> AccountSharedData {
    let mut data = vec![0; len];
    wincode::serialize_into(&mut data[..], &VoteStateVersions::new_v4(state)).unwrap();
    AccountSharedData::new_with_data(100_000_000, data, &id())
}

#[allow(clippy::too_many_arguments)]
fn run(
    account: AccountSharedData,
    voter: Pubkey,
    coordinates: (i32, i32, i32),
    enabled: bool,
    signed: bool,
    writable: bool,
    trailing: &[u8],
    expected: Result<(), InstructionError>,
) -> AccountSharedData {
    let vote_key = Pubkey::new_unique();
    let mut instruction = update_location(
        &vote_key,
        &voter,
        coordinates.0,
        coordinates.1,
        coordinates.2,
    );
    instruction.data.extend_from_slice(trailing);
    instruction.accounts[0].is_writable = writable;
    instruction.accounts[1].is_signer = signed;
    let clock = Clock {
        epoch: EPOCH,
        ..Clock::default()
    };
    let clock = AccountSharedData::new_data_with_space(
        1,
        &clock,
        solana_clock::SIZE,
        &solana_sdk_ids::sysvar::id(),
    )
    .unwrap();
    mock_process_instruction_with_feature_set(
        &id(),
        &instruction.data,
        vec![
            (vote_key, account),
            (voter, AccountSharedData::default()),
            (solana_sdk_ids::sysvar::clock::id(), clock),
        ],
        instruction.accounts,
        expected,
        Entrypoint::register,
        |_| {},
        |_| {},
        &SVMFeatureSet {
            validator_location_registration: enabled,
            ..SVMFeatureSet::all_enabled()
        },
    )
    .remove(0)
}

#[test]
fn registration_converts_v4_and_updates_v5() {
    let voter = Pubkey::new_unique();
    let mut state = v4(voter);
    state.pending_delegator_rewards = 123;
    state.epoch_credits = (0..10)
        .map(|epoch| (epoch, epoch * 2 + 2, epoch * 2))
        .collect();
    let original = account(state.clone(), VoteStateV4::size_of());
    let result = run(
        original.clone(),
        voter,
        FRANKFURT,
        true,
        true,
        true,
        &[0xff; 32],
        Ok(()),
    );
    assert_eq!(result.lamports(), original.lamports());
    assert_eq!(result.data().len(), VOTE_STATE_V5_SIZE);
    let registered = VoteStateV5::from_account_data_unaligned(result.data()).unwrap();
    assert_eq!(
        registered.location,
        Location::try_new(FRANKFURT.0, FRANKFURT.1, FRANKFURT.2).unwrap()
    );
    assert_eq!(registered.pending_delegator_rewards, 123);
    assert_eq!(registered.node_pubkey, state.node_pubkey);
    // Lookup caches the current voter and persists the purge of epochs 0 and 1.
    assert_eq!(
        registered
            .authorized_voters_ref()
            .0
            .iter()
            .map(|entry| entry.epoch)
            .collect::<Vec<_>>(),
        vec![9, 10]
    );
    assert_eq!(registered.accumulated_lamports_ref().0.len(), 5);
    assert!(
        result.data()[std::mem::size_of::<VoteStateV5>()..]
            .iter()
            .all(|byte| *byte == 0)
    );
    let result = run(result, voter, TOKYO, true, true, true, &[], Ok(()));
    let updated = VoteStateV5::from_account_data_unaligned(result.data()).unwrap();
    let mut expected = registered;
    expected.location = Location::try_new(TOKYO.0, TOKYO.1, TOKYO.2).unwrap();
    assert_eq!(updated, expected);
}

#[test]
fn registration_errors_and_precedence() {
    let voter = Pubkey::new_unique();
    let valid = account(v4(voter), VoteStateV4::size_of());
    let cases = [
        (
            false,
            false,
            true,
            FRANKFURT,
            InstructionError::InvalidInstructionData,
        ),
        (
            true,
            false,
            true,
            (0, 0, 0),
            InstructionError::MissingRequiredSignature,
        ),
        (
            true,
            true,
            true,
            (0, 0, 0),
            InstructionError::InvalidArgument,
        ),
        (
            true,
            true,
            true,
            (i32::MIN, 0, 0),
            InstructionError::InvalidArgument,
        ),
        (
            true,
            true,
            false,
            FRANKFURT,
            InstructionError::ReadonlyDataModified,
        ),
    ];
    for (enabled, signed, writable, coords, error) in cases {
        let result = run(
            valid.clone(),
            voter,
            coords,
            enabled,
            signed,
            writable,
            &[],
            Err(error),
        );
        assert_eq!(result, valid);
    }
    run(
        valid.clone(),
        Pubkey::new_unique(),
        FRANKFURT,
        true,
        true,
        true,
        &[],
        Err(InstructionError::MissingRequiredSignature),
    );
    let mut wrong_owner = valid.clone();
    wrong_owner.set_owner(Pubkey::new_unique());
    run(
        wrong_owner,
        voter,
        (0, 0, 0),
        false,
        false,
        true,
        &[],
        Err(InstructionError::InvalidAccountOwner),
    );
    let mut no_bls = v4(voter);
    no_bls.bls_pubkey_compressed = None;
    // Coordinate errors precede V4 conversion errors.
    run(
        account(no_bls.clone(), VoteStateV4::size_of()),
        voter,
        (0, 0, 0),
        true,
        true,
        true,
        &[],
        Err(InstructionError::InvalidArgument),
    );
    run(
        account(no_bls, VoteStateV4::size_of()),
        voter,
        FRANKFURT,
        true,
        true,
        true,
        &[],
        Err(InstructionError::InvalidAccountData),
    );
    for len in [VoteStateV4::size_of() - 1, VoteStateV4::size_of() + 1] {
        run(
            account(v4(voter), len),
            voter,
            FRANKFURT,
            true,
            true,
            true,
            &[],
            Err(InstructionError::InvalidAccountData),
        );
    }
    let mut many_voters = v4(voter);
    many_voters.authorized_voters = AuthorizedVoters::new(EPOCH, voter);
    for epoch in 11..15 {
        many_voters.authorized_voters.insert(epoch, voter);
    }
    run(
        account(many_voters, VoteStateV4::size_of()),
        voter,
        FRANKFURT,
        true,
        true,
        true,
        &[],
        Err(InstructionError::InvalidAccountData),
    );
    let uninitialized =
        AccountSharedData::new_with_data(100_000_000, vec![0; VoteStateV4::size_of()], &id());
    run(
        uninitialized,
        voter,
        FRANKFURT,
        true,
        true,
        true,
        &[],
        Err(InstructionError::InvalidAccountData),
    );
    let mut old = VoteStateV3::new(
        &VoteInit {
            authorized_voter: voter,
            ..VoteInit::default()
        },
        &Clock::default(),
    );
    old.authorized_voters.insert(EPOCH, voter);
    let mut bytes = vec![0; VoteStateV4::size_of()];
    wincode::serialize_into(&mut bytes[..], &VoteStateVersions::new_v3(old)).unwrap();
    run(
        AccountSharedData::new_with_data(100_000_000, bytes, &id()),
        voter,
        FRANKFURT,
        true,
        true,
        true,
        &[],
        Err(InstructionError::InvalidAccountData),
    );
}

#[test]
fn malformed_location_instruction() {
    let voter = Pubkey::new_unique();
    let vote_key = Pubkey::new_unique();
    let mut data = 20u32.to_le_bytes().to_vec();
    for _ in 4..16 {
        mock_process_instruction_with_feature_set(
            &id(),
            &data,
            vec![(vote_key, account(v4(voter), VoteStateV4::size_of()))],
            vec![AccountMeta::new(vote_key, false)],
            Err(InstructionError::InvalidInstructionData),
            Entrypoint::register,
            |_| {},
            |_| {},
            &SVMFeatureSet::all_enabled(),
        );
        data.push(0);
    }
}

#[test]
fn v5_runtime_updates_preserve_histories_and_version() {
    use {
        crate::vote_state::{BlockTimestamp, LandedVote, Lockout, handler::VoteStateHandler},
        solana_vote_interface::state::vote_state_v5::Commission,
    };
    let voter = Pubkey::new_unique();
    let mut common = v4(voter);
    common.epoch_credits = vec![(0, 20, 0), (u64::MAX, u64::MAX, u64::MAX)];
    let mut state = VoteStateV5::from_vote_state_v4(
        common,
        Location::try_new(FRANKFURT.0, FRANKFURT.1, FRANKFURT.2).unwrap(),
        EPOCH,
    )
    .unwrap();
    state
        .inflation_rewards_commission_mut()
        .update(EPOCH + 1, Commission::new(456))
        .unwrap();
    state
        .block_revenue_commission_mut()
        .update(EPOCH + 1, Commission::new(789))
        .unwrap();
    state.accumulated_lamports_mut().update(9, 100).unwrap();
    let mut bytes = vec![0; VOTE_STATE_V5_SIZE];
    state.write_account_data(&mut bytes).unwrap();
    for epoch in 10..20 {
        let mut handler = VoteStateHandler::from_account_data(&bytes).unwrap();
        handler.set_votes(
            [LandedVote {
                lockout: Lockout::new(epoch * 100),
                latency: 0,
            }]
            .into(),
        );
        handler.set_root_slot(Some(epoch * 100 - 1));
        handler.set_last_timestamp(BlockTimestamp {
            slot: epoch * 100,
            timestamp: epoch as i64,
        });
        handler.increment_credits(epoch, 5);
        handler.serialize_into(&mut bytes).unwrap();
        state.accumulated_lamports_mut().update(epoch, 5).unwrap();
        state.set_latest_vote(Some(epoch * 100));
        state.set_root_slot(Some(epoch * 100 - 1));
        state.last_timestamp = BlockTimestamp {
            slot: epoch * 100,
            timestamp: epoch as i64,
        };
        assert_eq!(
            VoteStateV5::from_account_data_unaligned(&bytes).unwrap(),
            state
        );
    }
    // Re-registering must retain both commission schedules and reward history.
    let result = run(
        AccountSharedData::new_with_data(100_000_000, bytes, &id()),
        voter,
        TOKYO,
        true,
        true,
        true,
        &[],
        Ok(()),
    );
    let actual = VoteStateV5::from_account_data_unaligned(result.data()).unwrap();
    state.location = Location::try_new(TOKYO.0, TOKYO.1, TOKYO.2).unwrap();
    // Epoch 10 lookup caches the current voter and purges old voters.
    state.authorized_voters_mut().purge_before(9);
    state.authorized_voters_mut().update(10, voter).unwrap();
    assert_eq!(actual, state);
}

#[test]
fn v5_commission_updates_record_epochs_and_preserve_other_schedule() {
    use solana_vote_interface::instruction::{CommissionKind, VoteInstruction};
    let voter = Pubkey::new_unique();
    let mut common = v4(voter);
    common.authorized_withdrawer = voter;
    let state = VoteStateV5::from_vote_state_v4(
        common,
        Location::try_new(FRANKFURT.0, FRANKFURT.1, FRANKFURT.2).unwrap(),
        0,
    )
    .unwrap();
    let mut bytes = vec![0; VOTE_STATE_V5_SIZE];
    state.write_account_data(&mut bytes).unwrap();
    let key = Pubkey::new_unique();
    let mut account = AccountSharedData::new_with_data(100_000_000, bytes, &id());
    for (epoch, bps) in [(1, 100), (1, 200), (2, 300), (3, 400), (4, 500)] {
        let instruction = VoteInstruction::UpdateCommissionBps {
            commission_bps: bps,
            kind: CommissionKind::InflationRewards,
        };
        account = mock_process_instruction_with_feature_set(
            &id(),
            &wincode::serialize(&instruction).unwrap(),
            vec![
                (key, account),
                (voter, AccountSharedData::default()),
                (
                    solana_sdk_ids::sysvar::clock::id(),
                    AccountSharedData::new_data_with_space(
                        1,
                        &Clock {
                            epoch,
                            ..Clock::default()
                        },
                        solana_clock::SIZE,
                        &solana_sdk_ids::sysvar::id(),
                    )
                    .unwrap(),
                ),
            ],
            vec![
                AccountMeta::new(key, false),
                AccountMeta::new_readonly(voter, true),
            ],
            Ok(()),
            Entrypoint::register,
            |_| {},
            |_| {},
            &SVMFeatureSet::all_enabled(),
        )
        .remove(0);
    }
    let updated = VoteStateV5::from_account_data_unaligned(account.data()).unwrap();
    assert_eq!(
        updated.inflation_rewards_commission_ref().epochs,
        &[3, 4, 5, 6]
    );
    assert_eq!(
        updated
            .inflation_rewards_commission_ref()
            .bps
            .iter()
            .map(|c| c.get())
            .collect::<Vec<_>>(),
        vec![200, 300, 400, 500]
    );
    assert_eq!(
        updated.block_revenue_commission_ref().epochs,
        state.block_revenue_commission_ref().epochs
    );
    assert_eq!(
        updated.block_revenue_commission_ref().bps,
        state.block_revenue_commission_ref().bps
    );
}

#[test]
fn missing_clock_preserves_deserialization_error_order() {
    let vote_key = Pubkey::new_unique();
    let voter = Pubkey::new_unique();
    let instruction = update_location(&vote_key, &voter, FRANKFURT.0, FRANKFURT.1, FRANKFURT.2);
    for (account, error) in [
        (
            AccountSharedData::new_with_data(100_000_000, vec![0; VoteStateV4::size_of()], &id()),
            InstructionError::InvalidAccountData,
        ),
        (
            account(v4(voter), VoteStateV4::size_of()),
            InstructionError::UnsupportedSysvar,
        ),
    ] {
        mock_process_instruction_with_feature_set(
            &id(),
            &instruction.data,
            vec![(vote_key, account), (voter, AccountSharedData::default())],
            instruction.accounts.clone(),
            Err(error),
            Entrypoint::register,
            |_| {},
            |_| {},
            &SVMFeatureSet::all_enabled(),
        );
    }
}

#[test]
fn v5_reward_history_saturates_and_rolls_over() {
    use crate::vote_state::handler::VoteStateHandler;
    let voter = Pubkey::new_unique();
    let mut common = v4(voter);
    common.epoch_credits = vec![(9, u64::MAX, u64::MAX - 1)];
    let mut state = VoteStateV5::from_vote_state_v4(
        common,
        Location::try_new(FRANKFURT.0, FRANKFURT.1, FRANKFURT.2).unwrap(),
        0,
    )
    .unwrap();
    let mut bytes = vec![0; VOTE_STATE_V5_SIZE];
    state.write_account_data(&mut bytes).unwrap();
    for epoch in 10..12 {
        let mut handler = VoteStateHandler::from_account_data(&bytes).unwrap();
        handler.increment_credits(epoch, 1);
        handler.serialize_into(&mut bytes).unwrap();
        state.accumulated_lamports_mut().update(epoch, 1).unwrap();
        assert_eq!(
            VoteStateV5::from_account_data_unaligned(&bytes).unwrap(),
            state
        );
    }
}
