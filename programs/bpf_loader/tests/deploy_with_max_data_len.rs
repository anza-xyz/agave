use {
    common::{
        LoaderV3Features, create_sysvar_account, process_instruction_with_setup, truncate_data,
    },
    solana_account::{
        AccountSharedData, ReadableAccount, WritableAccount,
        state_traits::StateMutWincode as StateMut,
    },
    solana_clock::Clock,
    solana_instruction::AccountMeta,
    solana_instruction_error::InstructionError,
    solana_loader_v3_interface::{
        instruction::UpgradeableLoaderInstruction, state::UpgradeableLoaderState,
    },
    solana_program_runtime::program_cache_entry::ProgramCacheEntry,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_sbpf::program::BuiltinFunctionDefinition,
    solana_sdk_ids::{bpf_loader_upgradeable, native_loader, system_program, sysvar},
    solana_svm_type_overrides::sync::Arc,
    solana_system_interface::MAX_PERMITTED_DATA_LENGTH,
    std::{fs::File, io::Read},
};

mod common;

#[test]
fn test_bpf_loader_upgradeable_deploy_with_max_data_len() {
    let mut file = File::open("test_elfs/out/sbpfv3_return_ok.so").expect("file open failed");
    let mut elf = Vec::new();
    file.read_to_end(&mut elf).unwrap();
    const SLOT: u64 = 42;
    let payer_address = Pubkey::new_unique();
    let buffer_address = Pubkey::new_unique();
    let upgrade_authority_address = Pubkey::new_unique();

    fn get_accounts(
        payer_address: &Pubkey,
        buffer_address: &Pubkey,
        buffer_authority: &Pubkey,
        upgrade_authority_address: &Pubkey,
        elf: &[u8],
    ) -> (Vec<(Pubkey, AccountSharedData)>, Vec<AccountMeta>) {
        let loader_id = bpf_loader_upgradeable::id();
        let program_address = Pubkey::new_unique();
        let rent = Rent::default();
        let min_program_balance =
            1.max(rent.minimum_balance(UpgradeableLoaderState::size_of_program()));
        let min_programdata_balance =
            1.max(rent.minimum_balance(UpgradeableLoaderState::size_of_programdata(elf.len())));
        let (programdata_address, _) =
            Pubkey::find_program_address(&[program_address.as_ref()], &loader_id);
        let mut buffer_account = AccountSharedData::new(
            1,
            UpgradeableLoaderState::size_of_buffer(elf.len()),
            &bpf_loader_upgradeable::id(),
        );
        buffer_account
            .set_state(&UpgradeableLoaderState::Buffer {
                authority_address: Some(*buffer_authority),
            })
            .unwrap();
        buffer_account
            .data_as_mut_slice()
            .get_mut(UpgradeableLoaderState::size_of_buffer_metadata()..)
            .unwrap()
            .copy_from_slice(elf);
        let programdata_account = AccountSharedData::new(0, 0, &system_program::id());
        let mut program_account = AccountSharedData::new(
            min_program_balance,
            UpgradeableLoaderState::size_of_program(),
            &bpf_loader_upgradeable::id(),
        );
        program_account
            .set_state(&UpgradeableLoaderState::Uninitialized)
            .unwrap();
        let payer_account = AccountSharedData::new(
            min_programdata_balance.saturating_add(1),
            0,
            &system_program::id(),
        );
        let rent_account = create_sysvar_account(&rent);
        let clock_account = create_sysvar_account(&Clock {
            slot: SLOT,
            ..Clock::default()
        });
        let system_program_account = AccountSharedData::new(0, 0, &native_loader::id());
        let upgrade_authority_account = AccountSharedData::new(1, 0, &Pubkey::new_unique());
        let transaction_accounts = vec![
            (*payer_address, payer_account),
            (programdata_address, programdata_account),
            (program_address, program_account),
            (*buffer_address, buffer_account),
            (sysvar::rent::id(), rent_account),
            (sysvar::clock::id(), clock_account),
            (system_program::id(), system_program_account),
            (*upgrade_authority_address, upgrade_authority_account),
        ];
        let instruction_accounts = vec![
            AccountMeta {
                pubkey: *payer_address,
                is_signer: true,
                is_writable: true,
            },
            AccountMeta {
                pubkey: programdata_address,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: program_address,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: *buffer_address,
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
                pubkey: system_program::id(),
                is_signer: false,
                is_writable: false,
            },
            AccountMeta {
                pubkey: *upgrade_authority_address,
                is_signer: true,
                is_writable: false,
            },
        ];
        (transaction_accounts, instruction_accounts)
    }

    fn process_instruction(
        max_data_len: usize,
        transaction_accounts: Vec<(Pubkey, AccountSharedData)>,
        instruction_accounts: Vec<AccountMeta>,
        expected_result: Result<(), InstructionError>,
    ) -> Vec<AccountSharedData> {
        let instruction_data =
            bincode::serialize(&UpgradeableLoaderInstruction::DeployWithMaxDataLen {
                max_data_len,
            })
            .unwrap();
        process_instruction_with_setup(
            &bpf_loader_upgradeable::id(),
            &instruction_data,
            transaction_accounts,
            instruction_accounts,
            LoaderV3Features::all_enabled(),
            expected_result,
            |invoke_context| {
                // Register the system program for CPI support.
                invoke_context.program_cache_for_tx_batch.replenish(
                    system_program::id(),
                    Arc::new(ProgramCacheEntry::new_builtin(
                        solana_system_program::system_processor::Entrypoint::register,
                    )),
                );
            },
        )
    }

    // Case: Success
    let (transaction_accounts, instruction_accounts) = get_accounts(
        &payer_address,
        &buffer_address,
        &upgrade_authority_address,
        &upgrade_authority_address,
        &elf,
    );
    let programdata_address = instruction_accounts.get(1).unwrap().pubkey;
    let accounts = process_instruction(
        elf.len(),
        transaction_accounts,
        instruction_accounts,
        Ok(()),
    );
    let min_programdata_balance =
        Rent::default().minimum_balance(UpgradeableLoaderState::size_of_programdata(elf.len()));
    assert_eq!(min_programdata_balance, accounts.get(1).unwrap().lamports());
    assert_eq!(2, accounts.first().unwrap().lamports());
    assert_eq!(0, accounts.get(3).unwrap().lamports());
    assert_eq!(
        UpgradeableLoaderState::size_of_buffer(0),
        accounts.get(3).unwrap().data().len()
    );
    let state: UpgradeableLoaderState = accounts.get(1).unwrap().state().unwrap();
    assert_eq!(
        state,
        UpgradeableLoaderState::ProgramData {
            slot: SLOT,
            upgrade_authority_address: Some(upgrade_authority_address),
        }
    );
    for (i, byte) in accounts
        .get(1)
        .unwrap()
        .data()
        .get(
            UpgradeableLoaderState::size_of_programdata_metadata()
                ..UpgradeableLoaderState::size_of_programdata(elf.len()),
        )
        .unwrap()
        .iter()
        .enumerate()
    {
        assert_eq!(*elf.get(i).unwrap(), *byte);
    }
    let state: UpgradeableLoaderState = accounts.get(2).unwrap().state().unwrap();
    assert_eq!(
        state,
        UpgradeableLoaderState::Program {
            programdata_address,
        }
    );
    assert!(accounts.get(2).unwrap().executable());

    // Case: wrong authority
    let (mut transaction_accounts, mut instruction_accounts) = get_accounts(
        &payer_address,
        &buffer_address,
        &upgrade_authority_address,
        &upgrade_authority_address,
        &elf,
    );
    let invalid_upgrade_authority_address = Pubkey::new_unique();
    transaction_accounts.get_mut(7).unwrap().0 = invalid_upgrade_authority_address;
    instruction_accounts.get_mut(7).unwrap().pubkey = invalid_upgrade_authority_address;
    process_instruction(
        elf.len(),
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::IncorrectAuthority),
    );

    // Case: authority did not sign
    let (transaction_accounts, mut instruction_accounts) = get_accounts(
        &payer_address,
        &buffer_address,
        &upgrade_authority_address,
        &upgrade_authority_address,
        &elf,
    );
    instruction_accounts.get_mut(7).unwrap().is_signer = false;
    process_instruction(
        elf.len(),
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::MissingRequiredSignature),
    );

    // Case: Buffer account and payer account alias
    let (transaction_accounts, mut instruction_accounts) = get_accounts(
        &payer_address,
        &buffer_address,
        &upgrade_authority_address,
        &upgrade_authority_address,
        &elf,
    );
    *instruction_accounts.get_mut(0).unwrap() = instruction_accounts.get(3).unwrap().clone();
    process_instruction(
        elf.len(),
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::AccountBorrowFailed),
    );

    // Case: Program account not owned by loader
    //
    // Unlike `Upgrade`, `DeployWithMaxDataLen` has no explicit owner
    // check on the program account. Validation passes, and the failure
    // only surfaces at the end when the handler tries to mutate the
    // program's state — `set_state` requires the account to be owned by
    // the currently-executing program, so it trips
    // `ExternalAccountDataModified`.
    let (mut transaction_accounts, instruction_accounts) = get_accounts(
        &payer_address,
        &buffer_address,
        &upgrade_authority_address,
        &upgrade_authority_address,
        &elf,
    );
    transaction_accounts
        .get_mut(2)
        .unwrap()
        .1
        .set_owner(Pubkey::new_unique());
    process_instruction(
        elf.len(),
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::ExternalAccountDataModified),
    );

    // Case: Program account not writable
    //
    // `DeployWithMaxDataLen` also lacks an explicit writability check on
    // the program account, so the failure again surfaces at
    // `set_state`, this time via the writability guard: a non-writable
    // account yields `ReadonlyDataModified`.
    let (transaction_accounts, mut instruction_accounts) = get_accounts(
        &payer_address,
        &buffer_address,
        &upgrade_authority_address,
        &upgrade_authority_address,
        &elf,
    );
    instruction_accounts.get_mut(2).unwrap().is_writable = false;
    process_instruction(
        elf.len(),
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::ReadonlyDataModified),
    );

    // Case: Program account already initialized
    let (mut transaction_accounts, instruction_accounts) = get_accounts(
        &payer_address,
        &buffer_address,
        &upgrade_authority_address,
        &upgrade_authority_address,
        &elf,
    );
    transaction_accounts
        .get_mut(2)
        .unwrap()
        .1
        .set_state(&UpgradeableLoaderState::Program {
            programdata_address: Pubkey::new_unique(),
        })
        .unwrap();
    process_instruction(
        elf.len(),
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::AccountAlreadyInitialized),
    );

    // Case: Program account too small
    let (mut transaction_accounts, instruction_accounts) = get_accounts(
        &payer_address,
        &buffer_address,
        &upgrade_authority_address,
        &upgrade_authority_address,
        &elf,
    );
    truncate_data(&mut transaction_accounts.get_mut(2).unwrap().1, 5);
    process_instruction(
        elf.len(),
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::AccountDataTooSmall),
    );

    // Case: Program account not rent-exempt
    let (mut transaction_accounts, instruction_accounts) = get_accounts(
        &payer_address,
        &buffer_address,
        &upgrade_authority_address,
        &upgrade_authority_address,
        &elf,
    );
    transaction_accounts.get_mut(2).unwrap().1.set_lamports(1);
    process_instruction(
        elf.len(),
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::ExecutableAccountNotRentExempt),
    );

    // Case: ProgramData address not derived
    let (mut transaction_accounts, mut instruction_accounts) = get_accounts(
        &payer_address,
        &buffer_address,
        &upgrade_authority_address,
        &upgrade_authority_address,
        &elf,
    );
    let invalid_programdata_address = Pubkey::new_unique();
    transaction_accounts.get_mut(1).unwrap().0 = invalid_programdata_address;
    instruction_accounts.get_mut(1).unwrap().pubkey = invalid_programdata_address;
    process_instruction(
        elf.len(),
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::InvalidArgument),
    );

    // Case: Buffer account not initialized
    let (mut transaction_accounts, instruction_accounts) = get_accounts(
        &payer_address,
        &buffer_address,
        &upgrade_authority_address,
        &upgrade_authority_address,
        &elf,
    );
    transaction_accounts
        .get_mut(3)
        .unwrap()
        .1
        .set_state(&UpgradeableLoaderState::Uninitialized)
        .unwrap();
    process_instruction(
        elf.len(),
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::InvalidArgument),
    );

    // Case: Buffer account not writable
    for buffer_balance in [0, 1_000_000, 15 * 1_000_000_000] {
        let (mut transaction_accounts, mut instruction_accounts) = get_accounts(
            &payer_address,
            &buffer_address,
            &upgrade_authority_address,
            &upgrade_authority_address,
            &elf,
        );
        transaction_accounts
            .get_mut(3)
            .unwrap()
            .1
            .set_lamports(buffer_balance);
        instruction_accounts.get_mut(3).unwrap().is_writable = false;
        process_instruction(
            elf.len(),
            transaction_accounts,
            instruction_accounts,
            Err(InstructionError::InvalidArgument),
        );
    }

    // Case: Buffer account not owned by loader: lamports scenario
    //
    // In `DeployWithMaxDataLen`, the buffer's lamports are drained to the
    // payer before the payer is debited for the programdata's rent. Then,
    // the buffer's data is set to `size_of_buffer(0)`.
    let (mut transaction_accounts, instruction_accounts) = get_accounts(
        &payer_address,
        &buffer_address,
        &upgrade_authority_address,
        &upgrade_authority_address,
        &elf,
    );
    {
        // Let's make sure the programdata requires a top-up.
        let required_rent =
            Rent::default().minimum_balance(UpgradeableLoaderState::size_of_programdata(elf.len()));
        let programdata = &transaction_accounts.get(1).unwrap().1;
        assert!(programdata.lamports() < required_rent);
        let buffer = &mut transaction_accounts.get_mut(3).unwrap().1;
        buffer.set_owner(Pubkey::new_unique());
        buffer.set_lamports(required_rent);
    }
    process_instruction(
        elf.len(),
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::IncorrectProgramId),
    );

    // Case: Buffer account not owned by loader: shrink scenario
    //
    // Same as the above case, but give the buffer a lamports balance of
    // `0`, rendering its balance "unchanged" by the drain operation.
    let (mut transaction_accounts, instruction_accounts) = get_accounts(
        &payer_address,
        &buffer_address,
        &upgrade_authority_address,
        &upgrade_authority_address,
        &elf,
    );
    {
        // Set the buffer's lamports to zero.
        let buffer = &mut transaction_accounts.get_mut(3).unwrap().1;
        buffer.set_owner(Pubkey::new_unique());
        buffer.set_lamports(0);
    }
    process_instruction(
        elf.len(),
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::IncorrectProgramId),
    );

    // Case: Buffer account not owned by loader: no-op scenario
    //
    // Same as the above case, but also truncate the buffer's data to
    // `size_of_buffer(0)` - just the buffer metadata, no ELF - rendering
    // the closing resize "unchanged" as well.
    let (mut transaction_accounts, instruction_accounts) = get_accounts(
        &payer_address,
        &buffer_address,
        &upgrade_authority_address,
        &upgrade_authority_address,
        &elf,
    );
    {
        // Empty the buffer (metadata only) and zero its lamports.
        let buffer = &mut transaction_accounts.get_mut(3).unwrap().1;
        buffer.set_owner(Pubkey::new_unique());
        buffer.set_lamports(0);
        truncate_data(buffer, UpgradeableLoaderState::size_of_buffer(0));
    }
    process_instruction(
        elf.len(),
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::IncorrectProgramId),
    );

    // Case: Max data length too small for Buffer data
    let (transaction_accounts, instruction_accounts) = get_accounts(
        &payer_address,
        &buffer_address,
        &upgrade_authority_address,
        &upgrade_authority_address,
        &elf,
    );
    process_instruction(
        elf.len().saturating_sub(1),
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::AccountDataTooSmall),
    );

    // Case: Max data length too large
    let (transaction_accounts, instruction_accounts) = get_accounts(
        &payer_address,
        &buffer_address,
        &upgrade_authority_address,
        &upgrade_authority_address,
        &elf,
    );
    process_instruction(
        MAX_PERMITTED_DATA_LENGTH as usize,
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::InvalidArgument),
    );

    // Case: Mismatched buffer authority
    let (transaction_accounts, instruction_accounts) = get_accounts(
        &payer_address,
        &buffer_address,
        &buffer_address,
        &upgrade_authority_address,
        &elf,
    );
    process_instruction(
        elf.len(),
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::IncorrectAuthority),
    );

    // Case: No buffer authority
    let (mut transaction_accounts, instruction_accounts) = get_accounts(
        &payer_address,
        &buffer_address,
        &buffer_address,
        &upgrade_authority_address,
        &elf,
    );
    transaction_accounts
        .get_mut(3)
        .unwrap()
        .1
        .set_state(&UpgradeableLoaderState::Buffer {
            authority_address: None,
        })
        .unwrap();
    process_instruction(
        elf.len(),
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::IncorrectAuthority),
    );

    // Case: Deploy SBPFv0
    let mut file = File::open("test_elfs/out/sbpfv0_verifier_err.so").expect("file open failed");
    let mut elf = Vec::new();
    file.read_to_end(&mut elf).unwrap();
    let (transaction_accounts, instruction_accounts) = get_accounts(
        &payer_address,
        &buffer_address,
        &upgrade_authority_address,
        &upgrade_authority_address,
        &elf,
    );
    process_instruction(
        elf.len(),
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::InvalidAccountData),
    );
}
