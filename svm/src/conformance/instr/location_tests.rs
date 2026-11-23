#![allow(clippy::arithmetic_side_effects)]
//! SIMD-0674 fixtures. Set EMIT_CONFORMANCE_FIXTURES to export the cases.
use {
    super::{InstrContext, execute_instr},
    crate::conformance::programs::keyed_account_for_builtin_pubkey,
    solana_account::Account,
    solana_clock::Clock,
    solana_instruction::{AccountMeta, Instruction},
    solana_instruction_error::InstructionError,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_svm_feature_set::SVMFeatureSet,
    solana_vote_program::vote_state::{
        VoteInit, VoteStateV3, VoteStateV4, VoteStateVersions,
        vote_state_v5::{Location, VoteStateV5},
    },
};

const VOTE: Pubkey = Pubkey::new_from_array([101; 32]);
const VOTER: Pubkey = Pubkey::new_from_array([102; 32]);
const EPOCH: u64 = 10;

fn v4() -> VoteStateV4 {
    let mut state = VoteStateV4::new_with_defaults(
        &VOTE,
        &VoteInit {
            authorized_voter: VOTER,
            ..VoteInit::default()
        },
        &Clock::default(),
    );
    state.bls_pubkey_compressed = Some([1; 48]);
    state.authorized_voters.insert(1, VOTER);
    state.authorized_voters.insert(9, VOTER);
    state
}

fn v4_bytes(state: VoteStateV4, len: usize) -> Vec<u8> {
    let mut bytes = vec![0; len];
    wincode::serialize_into(&mut bytes[..], &VoteStateVersions::new_v4(state)).unwrap();
    bytes
}

fn context(bytes: Vec<u8>, coords: (i32, i32, i32)) -> InstrContext {
    let mut data = 20u32.to_le_bytes().to_vec();
    for coordinate in [coords.0, coords.1, coords.2] {
        data.extend_from_slice(&coordinate.to_le_bytes());
    }
    let mut accounts = vec![
        (
            VOTE,
            Account {
                lamports: 100_000_000,
                data: bytes,
                owner: solana_sdk_ids::vote::id(),
                ..Account::default()
            },
        ),
        (
            VOTER,
            Account {
                lamports: 1,
                ..Account::default()
            },
        ),
        keyed_account_for_builtin_pubkey(&solana_sdk_ids::vote::id()).unwrap(),
    ];
    for (key, data) in [
        (
            solana_sdk_ids::sysvar::clock::id(),
            wincode::serialize(&Clock {
                epoch: EPOCH,
                ..Clock::default()
            })
            .unwrap(),
        ),
        (
            solana_sdk_ids::sysvar::rent::id(),
            wincode::serialize(&Rent::default()).unwrap(),
        ),
    ] {
        accounts.push((
            key,
            Account {
                lamports: 1,
                data,
                owner: solana_sdk_ids::sysvar::id(),
                ..Account::default()
            },
        ));
    }
    InstrContext::new_with_default_budget(
        SVMFeatureSet::all_enabled(),
        accounts,
        Instruction::new_with_bytes(
            solana_sdk_ids::vote::id(),
            &data,
            vec![
                AccountMeta::new(VOTE, false),
                AccountMeta::new_readonly(VOTER, true),
            ],
        ),
    )
}

#[test]
fn validator_location_registration_conformance() {
    let coords = (4_051_542, 618_526, 4_870_645);
    let size = VoteStateV4::size_of();
    let valid = context(v4_bytes(v4(), size), coords);
    let effects = execute_instr(&valid);
    assert_eq!(effects.result, None);
    let registered = &effects.get_account(&VOTE).unwrap().data;
    let state = VoteStateV5::from_account_data_unaligned(registered).unwrap();
    assert_eq!(
        state
            .authorized_voters_ref()
            .0
            .iter()
            .map(|v| v.epoch)
            .collect::<Vec<_>>(),
        vec![9, 10]
    );
    let mut update = context(registered.clone(), (-3_955_214, 3_355_452, 3_699_409));
    update.instruction.data.extend_from_slice(&[255; 9]);
    let effects = execute_instr(&update);
    assert_eq!(effects.result, None);
    let updated =
        VoteStateV5::from_account_data_unaligned(&effects.get_account(&VOTE).unwrap().data)
            .unwrap();
    assert_eq!(
        updated.location,
        Location::try_new(-3_955_214, 3_355_452, 3_699_409).unwrap()
    );

    let mut cases = Vec::new();
    let mut disabled = context(v4_bytes(v4(), size), coords);
    disabled.feature_set.validator_location_registration = false;
    cases.push((disabled, InstructionError::InvalidInstructionData));
    let mut wrong_owner = context(v4_bytes(v4(), size), coords);
    wrong_owner.accounts[0].1.owner = solana_sdk_ids::system_program::id();
    wrong_owner.feature_set.validator_location_registration = false;
    cases.push((wrong_owner, InstructionError::InvalidAccountOwner));
    cases.push((
        context(vec![0; size], coords),
        InstructionError::InvalidAccountData,
    ));
    let mut unsigned = context(v4_bytes(v4(), size), (0, 0, 0));
    unsigned.instruction.accounts[1].is_signer = false;
    cases.push((unsigned, InstructionError::MissingRequiredSignature));
    let mut wrong_voter = v4();
    wrong_voter
        .authorized_voters
        .insert(EPOCH, Pubkey::new_from_array([103; 32]));
    cases.push((
        context(v4_bytes(wrong_voter, size), coords),
        InstructionError::MissingRequiredSignature,
    ));
    cases.push((
        context(v4_bytes(v4(), size), (0, 0, 0)),
        InstructionError::InvalidArgument,
    ));
    let mut no_bls = v4();
    no_bls.bls_pubkey_compressed = None;
    cases.push((
        context(v4_bytes(no_bls, size), coords),
        InstructionError::InvalidAccountData,
    ));
    for len in [size - 1, size + 1] {
        cases.push((
            context(v4_bytes(v4(), len), coords),
            InstructionError::InvalidAccountData,
        ));
    }
    let mut too_many_voters = v4();
    for epoch in 10..15 {
        too_many_voters.authorized_voters.insert(epoch, VOTER);
    }
    cases.push((
        context(v4_bytes(too_many_voters, size), coords),
        InstructionError::InvalidAccountData,
    ));
    let mut too_many_credits = v4();
    too_many_credits.epoch_credits = (0..65).map(|epoch| (epoch, epoch + 1, epoch)).collect();
    cases.push((
        context(v4_bytes(too_many_credits, size), coords),
        InstructionError::InvalidAccountData,
    ));
    let mut readonly = context(v4_bytes(v4(), size), coords);
    readonly.instruction.accounts[0].is_writable = false;
    cases.push((readonly, InstructionError::ReadonlyDataModified));
    let old = VoteStateV3::new(
        &VoteInit {
            authorized_voter: VOTER,
            ..VoteInit::default()
        },
        &Clock::default(),
    );
    for versioned in [
        VoteStateVersions::V1_14_11(Box::new(old.clone().into())),
        VoteStateVersions::new_v3(old),
    ] {
        let mut bytes = vec![0; size];
        wincode::serialize_into(&mut bytes[..], &versioned).unwrap();
        cases.push((context(bytes, coords), InstructionError::InvalidAccountData));
    }
    let mut malformed = context(v4_bytes(v4(), size), coords);
    malformed.instruction.data.truncate(15);
    cases.push((malformed, InstructionError::InvalidInstructionData));
    let mut invalid_v5 = registered.clone();
    invalid_v5[132..144].fill(0);
    cases.push((
        context(invalid_v5, coords),
        InstructionError::InvalidAccountData,
    ));
    let mut invalid_tail = registered.clone();
    invalid_tail[950] = 1;
    cases.push((
        context(invalid_tail, coords),
        InstructionError::InvalidAccountData,
    ));
    // V5 validation rejects each malformed bounded field and nonzero tail.
    for offset in [152, 156] {
        for length in [0u32, 5] {
            let mut bytes = registered.clone();
            bytes[offset..offset + 4].copy_from_slice(&length.to_le_bytes());
            cases.push((context(bytes, coords), InstructionError::InvalidAccountData));
        }
    }
    for (offset, length) in [(304, 5u32), (308, 6)] {
        let mut bytes = registered.clone();
        bytes[offset..offset + 4].copy_from_slice(&length.to_le_bytes());
        cases.push((context(bytes, coords), InstructionError::InvalidAccountData));
    }
    for offset in [168, 194, 208, 234, 392, 472, 608, 616, 618] {
        let mut bytes = registered.clone();
        bytes[offset] ^= 1;
        cases.push((context(bytes, coords), InstructionError::InvalidAccountData));
    }
    let mut wrong_size = registered.clone();
    wrong_size.push(0);
    cases.push((
        context(wrong_size, coords),
        InstructionError::InvalidAccountData,
    ));
    for (context, expected) in cases {
        let effects = execute_instr(&context);
        assert_eq!(effects.result, Some(expected));
        assert_eq!(effects.get_account(&VOTE).unwrap(), &context.accounts[0].1);
    }
}

#[test]
fn validator_location_registration_fixtures_replay() {
    use {
        crate::conformance::instr::effects::apply_firedancer_transformations, prost::Message,
        protosol::protos::InstrFixture,
    };
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/validator-location/instr");
    let mut count = 0;
    for path in std::fs::read_dir(directory).unwrap() {
        let path = path.unwrap().path();
        if path.extension().is_none_or(|ext| ext != "fix") {
            continue;
        }
        let fixture = InstrFixture::decode(std::fs::read(&path).unwrap().as_slice()).unwrap();
        let context = InstrContext::from(fixture.input.unwrap());
        assert_eq!(
            apply_firedancer_transformations(&context, &execute_instr(&context)),
            fixture.output.unwrap(),
            "{}",
            path.display()
        );
        count += 1;
    }
    assert!(count >= 35);
}
