use {
    common::{LoaderV3Features, create_sysvar_account, process_instruction_with_setup},
    solana_account::{
        AccountSharedData, ReadableAccount, WritableAccount, state_traits::StateMutWincode as _,
    },
    solana_clock::Clock,
    solana_instruction::Instruction,
    solana_instruction_error::InstructionError,
    solana_loader_v3_interface::{
        instruction::{MINIMUM_EXTEND_PROGRAM_BYTES, extend_program},
        state::UpgradeableLoaderState,
    },
    solana_program_runtime::program_cache_entry::ProgramCacheEntry,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_sbpf::program::BuiltinFunctionDefinition,
    solana_sdk_ids::{bpf_loader_upgradeable::id, native_loader, sysvar},
    solana_svm_type_overrides::sync::Arc,
    solana_system_interface::{
        MAX_PERMITTED_DATA_LENGTH, error::SystemError, program as system_program,
    },
    std::{fs::File, io::Read},
    test_case::test_case,
};

mod common;

fn process_instruction(
    loader_v3_features: LoaderV3Features,
    transaction_accounts: Vec<(Pubkey, AccountSharedData)>,
    instruction: Instruction,
    expected_result: Result<(), InstructionError>,
) -> Vec<AccountSharedData> {
    process_instruction_with_setup(
        &instruction.program_id,
        &instruction.data,
        transaction_accounts,
        instruction.accounts,
        loader_v3_features,
        expected_result,
        |invoke_context| {
            invoke_context.program_cache_for_tx_batch.replenish(
                system_program::id(),
                Arc::new(ProgramCacheEntry::new_builtin(
                    solana_system_program::system_processor::Entrypoint::register,
                )),
            );
        },
    )
}

fn create_upgradeable_loader_account(
    account_state: &UpgradeableLoaderState,
    account_data_len: usize,
) -> AccountSharedData {
    AccountSharedData::new_data_with_space(
        Rent::default().minimum_balance(account_data_len),
        account_state,
        account_data_len,
        &id(),
    )
    .expect("state failed to serialize into account data")
}

fn create_clock_account(slot: u64) -> AccountSharedData {
    create_sysvar_account(&Clock {
        slot,
        ..Clock::default()
    })
}

fn get_accounts(
    program: (Pubkey, AccountSharedData),
    programdata: (Pubkey, AccountSharedData),
    payer_address: &Pubkey,
) -> Vec<(Pubkey, AccountSharedData)> {
    vec![
        program,
        programdata,
        (
            *payer_address,
            AccountSharedData::new(10_000_000_000, 0, &system_program::id()),
        ),
        (sysvar::clock::id(), create_clock_account(1)),
        (sysvar::rent::id(), create_sysvar_account(&Rent::default())),
        (
            system_program::id(),
            AccountSharedData::new(0, 0, &native_loader::id()),
        ),
    ]
}

fn read_noop_elf() -> Vec<u8> {
    let mut file = File::open("test_elfs/out/noop.so").expect("file open failed");
    let mut data = Vec::new();
    file.read_to_end(&mut data).unwrap();
    data
}

fn features_without_simd_0431() -> LoaderV3Features {
    LoaderV3Features {
        minimum_extend_program_size: false,
        ..LoaderV3Features::all_enabled()
    }
}

#[test]
fn test_extend_program() {
    let data = read_noop_elf();
    let upgrade_authority = Pubkey::new_unique();
    let payer_address = Pubkey::new_unique();

    let program_address = Pubkey::new_unique();
    let (programdata_address, _) = Pubkey::find_program_address(&[program_address.as_ref()], &id());
    let programdata_data_offset = UpgradeableLoaderState::size_of_programdata_metadata();
    let program_data_len = data.len() + programdata_data_offset;
    let mut programdata_account = create_upgradeable_loader_account(
        &UpgradeableLoaderState::ProgramData {
            slot: 0,
            upgrade_authority_address: Some(upgrade_authority),
        },
        program_data_len,
    );
    programdata_account.data_as_mut_slice()[programdata_data_offset..].copy_from_slice(&data);
    let transaction_accounts = get_accounts(
        (
            program_address,
            create_upgradeable_loader_account(
                &UpgradeableLoaderState::Program {
                    programdata_address,
                },
                UpgradeableLoaderState::size_of_program(),
            ),
        ),
        (programdata_address, programdata_account),
        &payer_address,
    );

    const ADDITIONAL_BYTES: u32 = 42;
    let accounts = process_instruction(
        features_without_simd_0431(),
        transaction_accounts,
        extend_program(&program_address, Some(&payer_address), ADDITIONAL_BYTES),
        Ok(()),
    );
    assert_eq!(
        accounts.get(1).unwrap().data().len(),
        program_data_len + ADDITIONAL_BYTES as usize
    );
}

#[test]
fn test_failed_extend_twice_in_same_slot() {
    let data = read_noop_elf();
    let upgrade_authority = Pubkey::new_unique();
    let payer_address = Pubkey::new_unique();

    let program_address = Pubkey::new_unique();
    let (programdata_address, _) = Pubkey::find_program_address(&[program_address.as_ref()], &id());
    let programdata_data_offset = UpgradeableLoaderState::size_of_programdata_metadata();
    let program_data_len = data.len() + programdata_data_offset;
    let mut programdata_account = create_upgradeable_loader_account(
        &UpgradeableLoaderState::ProgramData {
            slot: 0,
            upgrade_authority_address: Some(upgrade_authority),
        },
        program_data_len,
    );
    programdata_account.data_as_mut_slice()[programdata_data_offset..].copy_from_slice(&data);
    let mut transaction_accounts = get_accounts(
        (
            program_address,
            create_upgradeable_loader_account(
                &UpgradeableLoaderState::Program {
                    programdata_address,
                },
                UpgradeableLoaderState::size_of_program(),
            ),
        ),
        (programdata_address, programdata_account),
        &payer_address,
    );

    const ADDITIONAL_BYTES: u32 = 42;
    let accounts = process_instruction(
        features_without_simd_0431(),
        transaction_accounts.clone(),
        extend_program(&program_address, Some(&payer_address), ADDITIONAL_BYTES),
        Ok(()),
    );
    assert_eq!(
        accounts.get(1).unwrap().data().len(),
        program_data_len + ADDITIONAL_BYTES as usize
    );
    for ((_, account), result) in transaction_accounts.iter_mut().zip(accounts) {
        *account = result;
    }

    // Extending the program in the same slot should fail
    process_instruction(
        features_without_simd_0431(),
        transaction_accounts,
        extend_program(&program_address, Some(&payer_address), ADDITIONAL_BYTES),
        Err(InstructionError::InvalidArgument),
    );
}

#[test]
fn test_extend_program_not_upgradeable() {
    let payer_address = Pubkey::new_unique();

    let program_address = Pubkey::new_unique();
    let (programdata_address, _) = Pubkey::find_program_address(&[program_address.as_ref()], &id());
    let transaction_accounts = get_accounts(
        (
            program_address,
            create_upgradeable_loader_account(
                &UpgradeableLoaderState::Program {
                    programdata_address,
                },
                UpgradeableLoaderState::size_of_program(),
            ),
        ),
        (
            programdata_address,
            create_upgradeable_loader_account(
                &UpgradeableLoaderState::ProgramData {
                    slot: 0,
                    upgrade_authority_address: None,
                },
                100,
            ),
        ),
        &payer_address,
    );

    process_instruction(
        features_without_simd_0431(),
        transaction_accounts,
        extend_program(&program_address, Some(&payer_address), 42),
        Err(InstructionError::Immutable),
    );
}

#[test]
fn test_extend_program_by_zero_bytes() {
    let upgrade_authority = Pubkey::new_unique();
    let payer_address = Pubkey::new_unique();

    let program_address = Pubkey::new_unique();
    let (programdata_address, _) = Pubkey::find_program_address(&[program_address.as_ref()], &id());
    let transaction_accounts = get_accounts(
        (
            program_address,
            create_upgradeable_loader_account(
                &UpgradeableLoaderState::Program {
                    programdata_address,
                },
                UpgradeableLoaderState::size_of_program(),
            ),
        ),
        (
            programdata_address,
            create_upgradeable_loader_account(
                &UpgradeableLoaderState::ProgramData {
                    slot: 0,
                    upgrade_authority_address: Some(upgrade_authority),
                },
                100,
            ),
        ),
        &payer_address,
    );

    process_instruction(
        features_without_simd_0431(),
        transaction_accounts,
        extend_program(&program_address, Some(&payer_address), 0),
        Err(InstructionError::InvalidInstructionData),
    );
}

#[test]
fn test_extend_program_past_max_size() {
    let upgrade_authority = Pubkey::new_unique();
    let payer_address = Pubkey::new_unique();

    let program_address = Pubkey::new_unique();
    let (programdata_address, _) = Pubkey::find_program_address(&[program_address.as_ref()], &id());
    let transaction_accounts = get_accounts(
        (
            program_address,
            create_upgradeable_loader_account(
                &UpgradeableLoaderState::Program {
                    programdata_address,
                },
                UpgradeableLoaderState::size_of_program(),
            ),
        ),
        (
            programdata_address,
            create_upgradeable_loader_account(
                &UpgradeableLoaderState::ProgramData {
                    slot: 0,
                    upgrade_authority_address: Some(upgrade_authority),
                },
                MAX_PERMITTED_DATA_LENGTH as usize,
            ),
        ),
        &payer_address,
    );

    process_instruction(
        features_without_simd_0431(),
        transaction_accounts,
        extend_program(&program_address, Some(&payer_address), 1),
        Err(InstructionError::InvalidRealloc),
    );
}

#[test]
fn test_extend_program_with_invalid_payer() {
    let rent = Rent::default();
    let payer_address = Pubkey::new_unique();
    let upgrade_authority_address = payer_address;

    let program_address = Pubkey::new_unique();
    let (programdata_address, _) = Pubkey::find_program_address(&[program_address.as_ref()], &id());
    let mut transaction_accounts = get_accounts(
        (
            program_address,
            create_upgradeable_loader_account(
                &UpgradeableLoaderState::Program {
                    programdata_address,
                },
                UpgradeableLoaderState::size_of_program(),
            ),
        ),
        (
            programdata_address,
            create_upgradeable_loader_account(
                &UpgradeableLoaderState::ProgramData {
                    slot: 0,
                    upgrade_authority_address: Some(upgrade_authority_address),
                },
                100,
            ),
        ),
        &payer_address,
    );

    let payer_with_sufficient_funds = Pubkey::new_unique();
    let payer_with_insufficient_funds = Pubkey::new_unique();
    let payer_with_invalid_owner = Pubkey::new_unique();
    transaction_accounts.extend([
        (
            payer_with_sufficient_funds,
            AccountSharedData::new(10_000_000_000, 0, &system_program::id()),
        ),
        (
            payer_with_insufficient_funds,
            AccountSharedData::new(rent.minimum_balance(0), 0, &system_program::id()),
        ),
        (
            payer_with_invalid_owner,
            AccountSharedData::new(rent.minimum_balance(0), 0, &id()),
        ),
    ]);

    process_instruction(
        features_without_simd_0431(),
        transaction_accounts.clone(),
        extend_program(&program_address, Some(&payer_with_insufficient_funds), 1024),
        Err(InstructionError::from(
            SystemError::ResultWithNegativeLamports,
        )),
    );

    process_instruction(
        features_without_simd_0431(),
        transaction_accounts.clone(),
        extend_program(&program_address, Some(&payer_with_invalid_owner), 1),
        Err(InstructionError::ExternalAccountLamportSpend),
    );

    let mut ix = extend_program(&program_address, Some(&payer_with_sufficient_funds), 1);

    {
        let payer_meta = ix
            .accounts
            .iter_mut()
            .find(|meta| meta.pubkey == payer_with_sufficient_funds)
            .expect("expected to find payer account meta");
        payer_meta.is_signer = false;
    }

    process_instruction(
        features_without_simd_0431(),
        transaction_accounts,
        ix,
        Err(InstructionError::PrivilegeEscalation),
    );
}

#[test]
fn test_extend_program_without_payer() {
    let rent = Rent::default();
    let data = read_noop_elf();
    let upgrade_authority = Pubkey::new_unique();
    let payer_address = Pubkey::new_unique();

    let program_address = Pubkey::new_unique();
    let (programdata_address, _) = Pubkey::find_program_address(&[program_address.as_ref()], &id());
    let programdata_data_offset = UpgradeableLoaderState::size_of_programdata_metadata();
    let program_data_len = data.len() + programdata_data_offset;
    let mut programdata_account = create_upgradeable_loader_account(
        &UpgradeableLoaderState::ProgramData {
            slot: 0,
            upgrade_authority_address: Some(upgrade_authority),
        },
        program_data_len,
    );
    programdata_account.data_as_mut_slice()[programdata_data_offset..].copy_from_slice(&data);
    let mut transaction_accounts = get_accounts(
        (
            program_address,
            create_upgradeable_loader_account(
                &UpgradeableLoaderState::Program {
                    programdata_address,
                },
                UpgradeableLoaderState::size_of_program(),
            ),
        ),
        (programdata_address, programdata_account),
        &payer_address,
    );

    process_instruction(
        features_without_simd_0431(),
        transaction_accounts.clone(),
        extend_program(&program_address, None, 1024),
        Err(InstructionError::MissingAccount),
    );

    const ADDITIONAL_BYTES: u32 = 42;
    let min_balance_increase_for_extend = rent
        .minimum_balance(ADDITIONAL_BYTES as usize)
        .saturating_sub(rent.minimum_balance(0));

    transaction_accounts
        .get_mut(1)
        .unwrap()
        .1
        .checked_add_lamports(min_balance_increase_for_extend)
        .unwrap();

    let accounts = process_instruction(
        features_without_simd_0431(),
        transaction_accounts,
        extend_program(&program_address, None, ADDITIONAL_BYTES),
        Ok(()),
    );
    assert_eq!(
        accounts.get(1).unwrap().data().len(),
        program_data_len + ADDITIONAL_BYTES as usize
    );
}

#[test]
fn test_extend_program_with_invalid_system_program() {
    let upgrade_authority = Pubkey::new_unique();
    let payer_address = Pubkey::new_unique();

    let program_address = Pubkey::new_unique();
    let (programdata_address, _) = Pubkey::find_program_address(&[program_address.as_ref()], &id());
    let program_data_len = 100;
    let transaction_accounts = get_accounts(
        (
            program_address,
            create_upgradeable_loader_account(
                &UpgradeableLoaderState::Program {
                    programdata_address,
                },
                UpgradeableLoaderState::size_of_program(),
            ),
        ),
        (
            programdata_address,
            create_upgradeable_loader_account(
                &UpgradeableLoaderState::ProgramData {
                    slot: 0,
                    upgrade_authority_address: Some(upgrade_authority),
                },
                program_data_len,
            ),
        ),
        &payer_address,
    );

    let mut ix = extend_program(&program_address, Some(&payer_address), 1);

    // Change system program to an invalid key
    {
        let system_program_meta = ix
            .accounts
            .iter_mut()
            .find(|meta| meta.pubkey == system_program::ID)
            .expect("expected to find system program account meta");
        system_program_meta.pubkey = Pubkey::new_unique();
    }

    process_instruction(
        features_without_simd_0431(),
        transaction_accounts,
        ix,
        Err(InstructionError::MissingAccount),
    );
}

#[test]
fn test_extend_program_with_mismatch_program_data() {
    let payer_address = Pubkey::new_unique();
    let upgrade_authority = Pubkey::new_unique();

    let program_address = Pubkey::new_unique();
    let (programdata_address, _) = Pubkey::find_program_address(&[program_address.as_ref()], &id());
    let mismatch_programdata_address = Pubkey::new_unique();
    let transaction_accounts = get_accounts(
        (
            program_address,
            create_upgradeable_loader_account(
                &UpgradeableLoaderState::Program {
                    programdata_address,
                },
                UpgradeableLoaderState::size_of_program(),
            ),
        ),
        (
            mismatch_programdata_address,
            create_upgradeable_loader_account(
                &UpgradeableLoaderState::ProgramData {
                    slot: 0,
                    upgrade_authority_address: Some(upgrade_authority),
                },
                100,
            ),
        ),
        &payer_address,
    );

    let mut ix = extend_program(&program_address, Some(&payer_address), 1);

    // Replace ProgramData account meta with invalid account
    {
        let program_data_meta = ix
            .accounts
            .iter_mut()
            .find(|meta| meta.pubkey == programdata_address)
            .expect("expected to find program data account meta");
        program_data_meta.pubkey = mismatch_programdata_address;
    }

    process_instruction(
        features_without_simd_0431(),
        transaction_accounts,
        ix,
        Err(InstructionError::InvalidArgument),
    );
}

#[test]
fn test_extend_program_with_readonly_program_data() {
    let payer_address = Pubkey::new_unique();
    let upgrade_authority = Pubkey::new_unique();

    let program_address = Pubkey::new_unique();
    let (programdata_address, _) = Pubkey::find_program_address(&[program_address.as_ref()], &id());
    let transaction_accounts = get_accounts(
        (
            program_address,
            create_upgradeable_loader_account(
                &UpgradeableLoaderState::Program {
                    programdata_address,
                },
                UpgradeableLoaderState::size_of_program(),
            ),
        ),
        (
            programdata_address,
            create_upgradeable_loader_account(
                &UpgradeableLoaderState::ProgramData {
                    slot: 0,
                    upgrade_authority_address: Some(upgrade_authority),
                },
                100,
            ),
        ),
        &payer_address,
    );

    let mut ix = extend_program(&program_address, Some(&payer_address), 1);

    // Demote ProgramData account meta to read-only
    {
        let program_data_meta = ix
            .accounts
            .iter_mut()
            .find(|meta| meta.pubkey == programdata_address)
            .expect("expected to find program data account meta");
        program_data_meta.is_writable = false;
    }

    process_instruction(
        features_without_simd_0431(),
        transaction_accounts,
        ix,
        Err(InstructionError::InvalidArgument),
    );
}

#[test]
fn test_extend_program_with_invalid_program_data_state() {
    let payer_address = Pubkey::new_unique();

    let program_address = Pubkey::new_unique();
    let (programdata_address, _) = Pubkey::find_program_address(&[program_address.as_ref()], &id());
    let transaction_accounts = get_accounts(
        (
            program_address,
            create_upgradeable_loader_account(
                &UpgradeableLoaderState::Program {
                    programdata_address,
                },
                UpgradeableLoaderState::size_of_program(),
            ),
        ),
        (
            programdata_address,
            create_upgradeable_loader_account(
                &UpgradeableLoaderState::Buffer {
                    authority_address: Some(payer_address),
                },
                100,
            ),
        ),
        &payer_address,
    );

    process_instruction(
        features_without_simd_0431(),
        transaction_accounts,
        extend_program(&program_address, Some(&payer_address), 1024),
        Err(InstructionError::InvalidAccountData),
    );
}

#[test_case(false; "zero")]
#[test_case(true; "uninitialized")]
fn test_extend_program_with_uninitialized_program_data(with_data: bool) {
    let payer_address = Pubkey::new_unique();

    let program_address = Pubkey::new_unique();
    let (programdata_address, _) = Pubkey::find_program_address(&[program_address.as_ref()], &id());
    let mut programdata_account = create_upgradeable_loader_account(
        &UpgradeableLoaderState::Uninitialized,
        UpgradeableLoaderState::size_of_uninitialized(),
    );
    if !with_data {
        programdata_account.set_data_from_slice(&[]);
    }
    let transaction_accounts = get_accounts(
        (
            program_address,
            create_upgradeable_loader_account(
                &UpgradeableLoaderState::Program {
                    programdata_address,
                },
                UpgradeableLoaderState::size_of_program(),
            ),
        ),
        (programdata_address, programdata_account),
        &payer_address,
    );

    process_instruction(
        features_without_simd_0431(),
        transaction_accounts,
        extend_program(&program_address, Some(&payer_address), 1024),
        Err(InstructionError::InvalidAccountData),
    );
}

#[test]
fn test_extend_program_with_invalid_program_data_owner() {
    let payer_address = Pubkey::new_unique();

    let program_address = Pubkey::new_unique();
    let (programdata_address, _) = Pubkey::find_program_address(&[program_address.as_ref()], &id());
    let invalid_owner = Pubkey::new_unique();
    let mut programdata_account = create_upgradeable_loader_account(
        &UpgradeableLoaderState::ProgramData {
            slot: 0,
            upgrade_authority_address: Some(payer_address),
        },
        100,
    );
    programdata_account.set_owner(invalid_owner);
    let transaction_accounts = get_accounts(
        (
            program_address,
            create_upgradeable_loader_account(
                &UpgradeableLoaderState::Program {
                    programdata_address,
                },
                UpgradeableLoaderState::size_of_program(),
            ),
        ),
        (programdata_address, programdata_account),
        &payer_address,
    );

    process_instruction(
        features_without_simd_0431(),
        transaction_accounts,
        extend_program(&program_address, Some(&payer_address), 1024),
        Err(InstructionError::InvalidAccountOwner),
    );
}

#[test]
fn test_extend_program_with_readonly_program() {
    let payer_address = Pubkey::new_unique();
    let upgrade_authority = Pubkey::new_unique();

    let program_address = Pubkey::new_unique();
    let (programdata_address, _) = Pubkey::find_program_address(&[program_address.as_ref()], &id());
    let transaction_accounts = get_accounts(
        (
            program_address,
            create_upgradeable_loader_account(
                &UpgradeableLoaderState::Program {
                    programdata_address,
                },
                UpgradeableLoaderState::size_of_program(),
            ),
        ),
        (
            programdata_address,
            create_upgradeable_loader_account(
                &UpgradeableLoaderState::ProgramData {
                    slot: 0,
                    upgrade_authority_address: Some(upgrade_authority),
                },
                100,
            ),
        ),
        &payer_address,
    );

    let mut ix = extend_program(&program_address, Some(&payer_address), 1);

    // Demote Program account meta to read-only
    {
        let program_meta = ix
            .accounts
            .iter_mut()
            .find(|meta| meta.pubkey == program_address)
            .expect("expected to find program account meta");
        program_meta.is_writable = false;
    }

    process_instruction(
        features_without_simd_0431(),
        transaction_accounts,
        ix,
        Err(InstructionError::InvalidArgument),
    );
}

#[test]
fn test_extend_program_with_invalid_program_owner() {
    let payer_address = Pubkey::new_unique();
    let upgrade_authority = Pubkey::new_unique();

    let program_address = Pubkey::new_unique();
    let (programdata_address, _) = Pubkey::find_program_address(&[program_address.as_ref()], &id());
    let invalid_owner = Pubkey::new_unique();
    let mut program_account = create_upgradeable_loader_account(
        &UpgradeableLoaderState::Program {
            programdata_address,
        },
        UpgradeableLoaderState::size_of_program(),
    );
    program_account.set_owner(invalid_owner);
    let transaction_accounts = get_accounts(
        (program_address, program_account),
        (
            programdata_address,
            create_upgradeable_loader_account(
                &UpgradeableLoaderState::ProgramData {
                    slot: 0,
                    upgrade_authority_address: Some(upgrade_authority),
                },
                100,
            ),
        ),
        &payer_address,
    );

    process_instruction(
        features_without_simd_0431(),
        transaction_accounts,
        extend_program(&program_address, Some(&payer_address), 1024),
        Err(InstructionError::InvalidAccountOwner),
    );
}

#[test]
fn test_extend_program_with_invalid_program_state() {
    let payer_address = Pubkey::new_unique();
    let upgrade_authority = Pubkey::new_unique();

    let program_address = Pubkey::new_unique();
    let (programdata_address, _) = Pubkey::find_program_address(&[program_address.as_ref()], &id());
    let transaction_accounts = get_accounts(
        (
            program_address,
            create_upgradeable_loader_account(
                &UpgradeableLoaderState::Buffer {
                    authority_address: Some(payer_address),
                },
                100,
            ),
        ),
        (
            programdata_address,
            create_upgradeable_loader_account(
                &UpgradeableLoaderState::ProgramData {
                    slot: 0,
                    upgrade_authority_address: Some(upgrade_authority),
                },
                100,
            ),
        ),
        &payer_address,
    );

    process_instruction(
        features_without_simd_0431(),
        transaction_accounts,
        extend_program(&program_address, Some(&payer_address), 1024),
        Err(InstructionError::InvalidAccountData),
    );
}

fn get_accounts_for_simd_0431_tests(
    program_address: &Pubkey,
    upgrade_authority_address: &Pubkey,
    payer_address: &Pubkey,
    programdata_len: usize,
) -> Vec<(Pubkey, AccountSharedData)> {
    let data = read_noop_elf();

    // Set up ProgramData state.
    let (programdata_address, _) = Pubkey::find_program_address(&[program_address.as_ref()], &id());
    let programdata_data_offset = UpgradeableLoaderState::size_of_programdata_metadata();
    let mut programdata_account = create_upgradeable_loader_account(
        &UpgradeableLoaderState::ProgramData {
            slot: 0,
            upgrade_authority_address: Some(*upgrade_authority_address),
        },
        programdata_len,
    );
    let end = programdata_data_offset.saturating_add(data.len());
    programdata_account.data_as_mut_slice()[programdata_data_offset..end].copy_from_slice(&data);

    get_accounts(
        (
            *program_address,
            create_upgradeable_loader_account(
                &UpgradeableLoaderState::Program {
                    programdata_address,
                },
                UpgradeableLoaderState::size_of_program(),
            ),
        ),
        (programdata_address, programdata_account),
        payer_address,
    )
}

#[test]
fn test_extend_program_minimum_size_requirement() {
    let program_address = Pubkey::new_unique();
    let upgrade_authority = Pubkey::new_unique();
    let payer_address = Pubkey::new_unique();
    let starting_programdata_len = (MINIMUM_EXTEND_PROGRAM_BYTES * 4) as usize;

    let mut transaction_accounts = get_accounts_for_simd_0431_tests(
        &program_address,
        &upgrade_authority,
        &payer_address,
        starting_programdata_len,
    );

    // Anything below the minimum size requirement should fail.
    for additional_bytes in [1, 69, 420, 10_000, MINIMUM_EXTEND_PROGRAM_BYTES - 1] {
        process_instruction(
            LoaderV3Features::all_enabled(),
            transaction_accounts.clone(),
            extend_program(&program_address, Some(&payer_address), additional_bytes),
            Err(InstructionError::InvalidArgument),
        );
    }

    // Anything at or above the minimum size requirement should succeed.
    let mut programdata_len = starting_programdata_len;
    for (additional_bytes, slot) in [
        (MINIMUM_EXTEND_PROGRAM_BYTES, 1),
        (MINIMUM_EXTEND_PROGRAM_BYTES + 1, 2),
    ] {
        transaction_accounts.get_mut(3).unwrap().1 = create_clock_account(slot);
        let accounts = process_instruction(
            LoaderV3Features::all_enabled(),
            transaction_accounts.clone(),
            extend_program(&program_address, Some(&payer_address), additional_bytes),
            Ok(()),
        );

        let expected_new_len = programdata_len + (additional_bytes as usize);
        assert_eq!(accounts.get(1).unwrap().data().len(), expected_new_len);
        programdata_len = expected_new_len;

        for ((_, account), result) in transaction_accounts.iter_mut().zip(accounts) {
            *account = result;
        }
    }
}

#[test]
fn test_extend_program_minimum_size_requirement_at_matching_headroom() {
    // Set the programdata length so that the headroom is exactly
    // MAX_PERMITTED_DATA_LENGTH - MINIMUM_EXTEND_PROGRAM_BYTES and ensure the
    // minimum requirement applies.

    let program_address = Pubkey::new_unique();
    let upgrade_authority = Pubkey::new_unique();
    let payer_address = Pubkey::new_unique();
    let programdata_len =
        (MAX_PERMITTED_DATA_LENGTH as usize) - (MINIMUM_EXTEND_PROGRAM_BYTES as usize);

    let transaction_accounts = get_accounts_for_simd_0431_tests(
        &program_address,
        &upgrade_authority,
        &payer_address,
        programdata_len,
    );

    // Anything below the minimum size requirement should fail.
    for additional_bytes in [1, 69, 420, 10_000, MINIMUM_EXTEND_PROGRAM_BYTES - 1] {
        process_instruction(
            LoaderV3Features::all_enabled(),
            transaction_accounts.clone(),
            extend_program(&program_address, Some(&payer_address), additional_bytes),
            Err(InstructionError::InvalidArgument),
        );
    }

    // Only exactly MINIMUM_EXTEND_PROGRAM_BYTES succeeds.
    let accounts = process_instruction(
        LoaderV3Features::all_enabled(),
        transaction_accounts,
        extend_program(
            &program_address,
            Some(&payer_address),
            MINIMUM_EXTEND_PROGRAM_BYTES,
        ),
        Ok(()),
    );
    assert_eq!(
        accounts.get(1).unwrap().data().len(),
        programdata_len + (MINIMUM_EXTEND_PROGRAM_BYTES as usize),
    );
}

#[test]
fn test_extend_program_near_max_headroom_requirement() {
    // Set the programdata length so that the headroom is less than
    // MINIMUM_EXTEND_PROGRAM_BYTES and ensure the *headroom* requirement
    // applies, and therefore not the minimum size requirement.

    let program_address = Pubkey::new_unique();
    let upgrade_authority = Pubkey::new_unique();
    let payer_address = Pubkey::new_unique();

    for headroom in [69, 420, 10_000, MINIMUM_EXTEND_PROGRAM_BYTES - 1] {
        let programdata_len = (MAX_PERMITTED_DATA_LENGTH as usize) - (headroom as usize);
        let transaction_accounts = get_accounts_for_simd_0431_tests(
            &program_address,
            &upgrade_authority,
            &payer_address,
            programdata_len,
        );

        // Anything below the headroom requirement should fail.
        let mut additional_bytes = 1;
        while additional_bytes < headroom - 1 {
            process_instruction(
                LoaderV3Features::all_enabled(),
                transaction_accounts.clone(),
                extend_program(&program_address, Some(&payer_address), additional_bytes),
                Err(InstructionError::InvalidArgument),
            );

            additional_bytes += headroom.div_ceil(5);
        }

        // The 10 KiB minimum should fail (too large).
        process_instruction(
            LoaderV3Features::all_enabled(),
            transaction_accounts.clone(),
            extend_program(
                &program_address,
                Some(&payer_address),
                MINIMUM_EXTEND_PROGRAM_BYTES,
            ),
            Err(InstructionError::InvalidRealloc),
        );

        // Only exactly `headroom` succeeds.
        let accounts = process_instruction(
            LoaderV3Features::all_enabled(),
            transaction_accounts,
            extend_program(&program_address, Some(&payer_address), headroom),
            Ok(()),
        );
        assert_eq!(
            accounts.get(1).unwrap().data().len(),
            programdata_len + (headroom as usize),
        );
    }
}
