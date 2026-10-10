use {
    common::{
        LoaderV3Features, create_sysvar_account, process_instruction_with_setup, truncate_data,
    },
    solana_account::{
        AccountSharedData, ReadableAccount, WritableAccount,
        state_traits::StateMutWincode as StateMut,
    },
    solana_bpf_loader_program::test_utils,
    solana_clock::Clock,
    solana_instruction::AccountMeta,
    solana_instruction_error::InstructionError,
    solana_loader_v3_interface::{
        instruction::UpgradeableLoaderInstruction, state::UpgradeableLoaderState,
    },
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_sdk_ids::{bpf_loader_upgradeable, sysvar},
    solana_system_interface::MAX_PERMITTED_DATA_LENGTH,
    std::{fs::File, io::Read},
    test_case::test_case,
};

mod common;

#[test_case(true; "simd_0433_enabled")]
#[test_case(false; "simd_0433_disabled")]
fn test_bpf_loader_upgradeable_upgrade(set_programdata_to_elf_length: bool) {
    let mut file = File::open("test_elfs/out/sbpfv3_return_ok.so").expect("file open failed");
    let mut elf_orig = Vec::new();
    file.read_to_end(&mut elf_orig).unwrap();
    let mut file = File::open("test_elfs/out/sbpfv3_return_err.so").expect("file open failed");
    let mut elf_new = Vec::new();
    file.read_to_end(&mut elf_new).unwrap();
    assert_ne!(elf_orig.len(), elf_new.len());
    const SLOT: u64 = 42;
    let buffer_address = Pubkey::new_unique();
    let upgrade_authority_address = Pubkey::new_unique();

    fn get_accounts(
        buffer_address: &Pubkey,
        buffer_authority: &Pubkey,
        upgrade_authority_address: &Pubkey,
        elf_orig: &[u8],
        elf_new: &[u8],
    ) -> (Vec<(Pubkey, AccountSharedData)>, Vec<AccountMeta>) {
        let loader_id = bpf_loader_upgradeable::id();
        let program_address = Pubkey::new_unique();
        let spill_address = Pubkey::new_unique();
        let rent = Rent::default();
        let min_program_balance =
            1.max(rent.minimum_balance(UpgradeableLoaderState::size_of_program()));
        let min_programdata_balance = 1.max(rent.minimum_balance(
            UpgradeableLoaderState::size_of_programdata(elf_orig.len().max(elf_new.len())),
        ));
        let (programdata_address, _) =
            Pubkey::find_program_address(&[program_address.as_ref()], &loader_id);
        let mut buffer_account = AccountSharedData::new(
            1,
            UpgradeableLoaderState::size_of_buffer(elf_new.len()),
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
            .copy_from_slice(elf_new);
        let mut programdata_account = AccountSharedData::new(
            min_programdata_balance,
            UpgradeableLoaderState::size_of_programdata(elf_orig.len().max(elf_new.len())),
            &bpf_loader_upgradeable::id(),
        );
        programdata_account
            .set_state(&UpgradeableLoaderState::ProgramData {
                slot: SLOT,
                upgrade_authority_address: Some(*upgrade_authority_address),
            })
            .unwrap();
        let mut program_account = AccountSharedData::new(
            min_program_balance,
            UpgradeableLoaderState::size_of_program(),
            &bpf_loader_upgradeable::id(),
        );
        program_account.set_executable(true);
        program_account
            .set_state(&UpgradeableLoaderState::Program {
                programdata_address,
            })
            .unwrap();
        let spill_account = AccountSharedData::new(0, 0, &Pubkey::new_unique());
        let rent_account = create_sysvar_account(&rent);
        let clock_account = create_sysvar_account(&Clock {
            slot: SLOT.saturating_add(1),
            ..Clock::default()
        });
        let upgrade_authority_account = AccountSharedData::new(1, 0, &Pubkey::new_unique());
        let transaction_accounts = vec![
            (programdata_address, programdata_account),
            (program_address, program_account),
            (*buffer_address, buffer_account),
            (spill_address, spill_account),
            (sysvar::rent::id(), rent_account),
            (sysvar::clock::id(), clock_account),
            (*upgrade_authority_address, upgrade_authority_account),
        ];
        let instruction_accounts = vec![
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
                pubkey: spill_address,
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
                pubkey: *upgrade_authority_address,
                is_signer: true,
                is_writable: false,
            },
        ];
        (transaction_accounts, instruction_accounts)
    }

    let process_instruction =
        |transaction_accounts: Vec<(Pubkey, AccountSharedData)>,
         instruction_accounts: Vec<AccountMeta>,
         expected_result: Result<(), InstructionError>| {
            let instruction_data =
                bincode::serialize(&UpgradeableLoaderInstruction::Upgrade).unwrap();
            process_instruction_with_setup(
                &bpf_loader_upgradeable::id(),
                &instruction_data,
                transaction_accounts,
                instruction_accounts,
                LoaderV3Features {
                    minimum_extend_program_size: true,
                    set_programdata_to_elf_length,
                },
                expected_result,
                |_invoke_context| {},
            )
        };

    // Case: Success
    let (transaction_accounts, instruction_accounts) = get_accounts(
        &buffer_address,
        &upgrade_authority_address,
        &upgrade_authority_address,
        &elf_orig,
        &elf_new,
    );
    let accounts = process_instruction(transaction_accounts, instruction_accounts, Ok(()));
    let starting_programdata_len =
        UpgradeableLoaderState::size_of_programdata(elf_orig.len().max(elf_new.len()));
    let starting_programdata_balance = Rent::default().minimum_balance(starting_programdata_len);
    let expected_programdata_len = if set_programdata_to_elf_length {
        UpgradeableLoaderState::size_of_programdata(elf_new.len())
    } else {
        starting_programdata_len
    };
    let expected_programdata_balance = Rent::default().minimum_balance(expected_programdata_len);
    assert_eq!(
        expected_programdata_len,
        accounts.first().unwrap().data().len()
    );
    assert_eq!(
        expected_programdata_balance,
        accounts.first().unwrap().lamports()
    );
    assert_eq!(0, accounts.get(2).unwrap().lamports());
    // The buffer's lone lamport, plus any rent freed by the retraction.
    assert_eq!(
        starting_programdata_balance
            .saturating_sub(expected_programdata_balance)
            .saturating_add(1),
        accounts.get(3).unwrap().lamports()
    );
    assert_eq!(
        UpgradeableLoaderState::size_of_buffer(0),
        accounts.get(2).unwrap().data().len()
    );
    let state: UpgradeableLoaderState = accounts.first().unwrap().state().unwrap();
    assert_eq!(
        state,
        UpgradeableLoaderState::ProgramData {
            slot: SLOT.saturating_add(1),
            upgrade_authority_address: Some(upgrade_authority_address)
        }
    );
    for (i, byte) in accounts
        .first()
        .unwrap()
        .data()
        .get(
            UpgradeableLoaderState::size_of_programdata_metadata()
                ..UpgradeableLoaderState::size_of_programdata(elf_new.len()),
        )
        .unwrap()
        .iter()
        .enumerate()
    {
        assert_eq!(*elf_new.get(i).unwrap(), *byte);
    }

    // Case: not upgradable
    let (mut transaction_accounts, instruction_accounts) = get_accounts(
        &buffer_address,
        &upgrade_authority_address,
        &upgrade_authority_address,
        &elf_orig,
        &elf_new,
    );
    transaction_accounts
        .get_mut(0)
        .unwrap()
        .1
        .set_state(&UpgradeableLoaderState::ProgramData {
            slot: SLOT,
            upgrade_authority_address: None,
        })
        .unwrap();
    process_instruction(
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::Immutable),
    );

    // Case: wrong authority
    let (mut transaction_accounts, mut instruction_accounts) = get_accounts(
        &buffer_address,
        &upgrade_authority_address,
        &upgrade_authority_address,
        &elf_orig,
        &elf_new,
    );
    let invalid_upgrade_authority_address = Pubkey::new_unique();
    transaction_accounts.get_mut(6).unwrap().0 = invalid_upgrade_authority_address;
    instruction_accounts.get_mut(6).unwrap().pubkey = invalid_upgrade_authority_address;
    process_instruction(
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::IncorrectAuthority),
    );

    // Case: authority did not sign
    let (transaction_accounts, mut instruction_accounts) = get_accounts(
        &buffer_address,
        &upgrade_authority_address,
        &upgrade_authority_address,
        &elf_orig,
        &elf_new,
    );
    instruction_accounts.get_mut(6).unwrap().is_signer = false;
    process_instruction(
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::MissingRequiredSignature),
    );

    // Case: Buffer account and spill account alias
    let (transaction_accounts, mut instruction_accounts) = get_accounts(
        &buffer_address,
        &upgrade_authority_address,
        &upgrade_authority_address,
        &elf_orig,
        &elf_new,
    );
    *instruction_accounts.get_mut(3).unwrap() = instruction_accounts.get(2).unwrap().clone();
    process_instruction(
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::AccountBorrowFailed),
    );

    // Case: Programdata account and spill account alias
    let (transaction_accounts, mut instruction_accounts) = get_accounts(
        &buffer_address,
        &upgrade_authority_address,
        &upgrade_authority_address,
        &elf_orig,
        &elf_new,
    );
    *instruction_accounts.get_mut(3).unwrap() = instruction_accounts.first().unwrap().clone();
    process_instruction(
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::AccountBorrowFailed),
    );

    // Case: Program account not a program
    let (transaction_accounts, mut instruction_accounts) = get_accounts(
        &buffer_address,
        &upgrade_authority_address,
        &upgrade_authority_address,
        &elf_orig,
        &elf_new,
    );
    *instruction_accounts.get_mut(1).unwrap() = instruction_accounts.get(2).unwrap().clone();
    let instruction_data = bincode::serialize(&UpgradeableLoaderInstruction::Upgrade).unwrap();

    process_instruction_with_setup(
        &bpf_loader_upgradeable::id(),
        &instruction_data,
        transaction_accounts.clone(),
        instruction_accounts.clone(),
        LoaderV3Features {
            minimum_extend_program_size: true,
            set_programdata_to_elf_length,
        },
        Err(InstructionError::InvalidAccountData),
        |invoke_context| {
            test_utils::load_all_invoked_programs(invoke_context);
        },
    );
    process_instruction(
        transaction_accounts.clone(),
        instruction_accounts.clone(),
        Err(InstructionError::InvalidAccountData),
    );

    // Case: Program account now owned by loader
    let (mut transaction_accounts, instruction_accounts) = get_accounts(
        &buffer_address,
        &upgrade_authority_address,
        &upgrade_authority_address,
        &elf_orig,
        &elf_new,
    );
    transaction_accounts
        .get_mut(1)
        .unwrap()
        .1
        .set_owner(Pubkey::new_unique());
    process_instruction(
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::IncorrectProgramId),
    );

    // Case: Program account not writable
    let (transaction_accounts, mut instruction_accounts) = get_accounts(
        &buffer_address,
        &upgrade_authority_address,
        &upgrade_authority_address,
        &elf_orig,
        &elf_new,
    );
    instruction_accounts.get_mut(1).unwrap().is_writable = false;
    process_instruction(
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::InvalidArgument),
    );

    // Case: Program account not initialized
    let (mut transaction_accounts, instruction_accounts) = get_accounts(
        &buffer_address,
        &upgrade_authority_address,
        &upgrade_authority_address,
        &elf_orig,
        &elf_new,
    );
    transaction_accounts
        .get_mut(1)
        .unwrap()
        .1
        .set_state(&UpgradeableLoaderState::Uninitialized)
        .unwrap();
    process_instruction(
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::InvalidAccountData),
    );

    // Case: Program ProgramData account mismatch
    let (mut transaction_accounts, mut instruction_accounts) = get_accounts(
        &buffer_address,
        &upgrade_authority_address,
        &upgrade_authority_address,
        &elf_orig,
        &elf_new,
    );
    let invalid_programdata_address = Pubkey::new_unique();
    transaction_accounts.get_mut(0).unwrap().0 = invalid_programdata_address;
    instruction_accounts.get_mut(0).unwrap().pubkey = invalid_programdata_address;
    process_instruction(
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::InvalidArgument),
    );

    // Case: Buffer account not initialized
    let (mut transaction_accounts, instruction_accounts) = get_accounts(
        &buffer_address,
        &upgrade_authority_address,
        &upgrade_authority_address,
        &elf_orig,
        &elf_new,
    );
    transaction_accounts
        .get_mut(2)
        .unwrap()
        .1
        .set_state(&UpgradeableLoaderState::Uninitialized)
        .unwrap();
    process_instruction(
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::InvalidArgument),
    );

    // Case: Buffer account not writable
    for buffer_balance in [0, 1_000_000, 15 * 1_000_000_000] {
        let (mut transaction_accounts, mut instruction_accounts) = get_accounts(
            &buffer_address,
            &upgrade_authority_address,
            &upgrade_authority_address,
            &elf_orig,
            &elf_new,
        );
        transaction_accounts
            .get_mut(2)
            .unwrap()
            .1
            .set_lamports(buffer_balance);
        instruction_accounts.get_mut(2).unwrap().is_writable = false;
        process_instruction(
            transaction_accounts,
            instruction_accounts,
            Err(InstructionError::InvalidArgument),
        );
    }

    // Case: Buffer account not owned by loader: lamports scenario
    //
    // In `Upgrade`, the buffer's lamports are used to fund the additional
    // programdata rent directly, with the rest spilled to the spill
    // account. Then, the buffer's data is set to `size_of_buffer(0)`.
    let (mut transaction_accounts, instruction_accounts) = get_accounts(
        &buffer_address,
        &upgrade_authority_address,
        &upgrade_authority_address,
        &elf_orig,
        &elf_new,
    );
    {
        // Let's make sure the programdata requires a top-up.
        let required_rent = |elf_len| {
            Rent::default().minimum_balance(UpgradeableLoaderState::size_of_programdata(elf_len))
        };
        let rent_orig = required_rent(elf_orig.len());
        let rent_new = required_rent(elf_new.len());
        let programdata = &mut transaction_accounts.first_mut().unwrap().1;
        programdata.set_lamports(rent_orig);
        let buffer = &mut transaction_accounts.get_mut(2).unwrap().1;
        buffer.set_owner(Pubkey::new_unique());
        buffer.set_lamports(rent_new);
    }
    process_instruction(
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::IncorrectProgramId),
    );

    // Case: Buffer account not owned by loader: shrink scenario
    //
    // Same as the above case, but give the buffer a lamports balance of
    // `0`, rendering its balance "unchanged" by the spill operation.
    let (mut transaction_accounts, instruction_accounts) = get_accounts(
        &buffer_address,
        &upgrade_authority_address,
        &upgrade_authority_address,
        &elf_orig,
        &elf_new,
    );
    {
        // Set the buffer's lamports to zero.
        let buffer = &mut transaction_accounts.get_mut(2).unwrap().1;
        buffer.set_owner(Pubkey::new_unique());
        buffer.set_lamports(0);
    }
    process_instruction(
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
        &buffer_address,
        &upgrade_authority_address,
        &upgrade_authority_address,
        &elf_orig,
        &elf_new,
    );
    {
        // Empty the buffer (metadata only) and zero its lamports.
        let buffer = &mut transaction_accounts.get_mut(2).unwrap().1;
        buffer.set_owner(Pubkey::new_unique());
        buffer.set_lamports(0);
        truncate_data(buffer, UpgradeableLoaderState::size_of_buffer(0));
    }
    process_instruction(
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::IncorrectProgramId),
    );

    // Case: Buffer account too big
    let (mut transaction_accounts, instruction_accounts) = get_accounts(
        &buffer_address,
        &upgrade_authority_address,
        &upgrade_authority_address,
        &elf_orig,
        &elf_new,
    );
    transaction_accounts.get_mut(2).unwrap().1 = AccountSharedData::new(
        1,
        UpgradeableLoaderState::size_of_buffer(elf_orig.len().max(elf_new.len()).saturating_add(1)),
        &bpf_loader_upgradeable::id(),
    );
    transaction_accounts
        .get_mut(2)
        .unwrap()
        .1
        .set_state(&UpgradeableLoaderState::Buffer {
            authority_address: Some(upgrade_authority_address),
        })
        .unwrap();
    process_instruction(
        transaction_accounts,
        instruction_accounts,
        if set_programdata_to_elf_length {
            Err(InstructionError::InsufficientFunds)
        } else {
            Err(InstructionError::AccountDataTooSmall)
        },
    );

    // Case: Buffer account too small
    let (mut transaction_accounts, instruction_accounts) = get_accounts(
        &buffer_address,
        &upgrade_authority_address,
        &upgrade_authority_address,
        &elf_orig,
        &elf_new,
    );
    transaction_accounts
        .get_mut(2)
        .unwrap()
        .1
        .set_state(&UpgradeableLoaderState::Buffer {
            authority_address: Some(upgrade_authority_address),
        })
        .unwrap();
    truncate_data(&mut transaction_accounts.get_mut(2).unwrap().1, 5);
    process_instruction(
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::InvalidAccountData),
    );

    // Case: Mismatched buffer and program authority
    let (transaction_accounts, instruction_accounts) = get_accounts(
        &buffer_address,
        &buffer_address,
        &upgrade_authority_address,
        &elf_orig,
        &elf_new,
    );
    process_instruction(
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::IncorrectAuthority),
    );

    // Case: No buffer authority
    let (mut transaction_accounts, instruction_accounts) = get_accounts(
        &buffer_address,
        &buffer_address,
        &upgrade_authority_address,
        &elf_orig,
        &elf_new,
    );
    transaction_accounts
        .get_mut(2)
        .unwrap()
        .1
        .set_state(&UpgradeableLoaderState::Buffer {
            authority_address: None,
        })
        .unwrap();
    process_instruction(
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::IncorrectAuthority),
    );

    // Case: No buffer and program authority
    let (mut transaction_accounts, instruction_accounts) = get_accounts(
        &buffer_address,
        &buffer_address,
        &upgrade_authority_address,
        &elf_orig,
        &elf_new,
    );
    transaction_accounts
        .get_mut(0)
        .unwrap()
        .1
        .set_state(&UpgradeableLoaderState::ProgramData {
            slot: SLOT,
            upgrade_authority_address: None,
        })
        .unwrap();
    transaction_accounts
        .get_mut(2)
        .unwrap()
        .1
        .set_state(&UpgradeableLoaderState::Buffer {
            authority_address: None,
        })
        .unwrap();
    process_instruction(
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::IncorrectAuthority),
    );

    // Case: Upgrade to SBPFv0
    let mut file = File::open("test_elfs/out/sbpfv0_verifier_err.so").expect("file open failed");
    let mut elf_new = Vec::new();
    file.read_to_end(&mut elf_new).unwrap();
    let (transaction_accounts, instruction_accounts) = get_accounts(
        &buffer_address,
        &upgrade_authority_address,
        &upgrade_authority_address,
        &elf_orig,
        &elf_new,
    );
    process_instruction(
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::InvalidAccountData),
    );
}
#[test]
fn test_bpf_loader_upgradeable_upgrade_simd_0433() {
    let mut file = File::open("test_elfs/out/sbpfv3_return_err.so").expect("file open failed");
    let mut elf_small = Vec::new();
    file.read_to_end(&mut elf_small).unwrap();
    let mut file = File::open("test_elfs/out/sbpfv3_return_ok.so").expect("file open failed");
    let mut elf_large = Vec::new();
    file.read_to_end(&mut elf_large).unwrap();
    assert!(elf_small.len() < elf_large.len());
    const SLOT: u64 = 42;
    let upgrade_authority_address = Pubkey::new_unique();

    fn get_accounts(
        upgrade_authority_address: &Pubkey,
        elf_orig: &[u8],
        elf_new: &[u8],
        programdata_len: usize,
        programdata_lamports: u64,
        buffer_len: usize,
        buffer_lamports: u64,
    ) -> (Vec<(Pubkey, AccountSharedData)>, Vec<AccountMeta>) {
        assert!(programdata_len >= UpgradeableLoaderState::size_of_programdata(elf_orig.len()));
        assert!(buffer_len >= UpgradeableLoaderState::size_of_buffer(elf_new.len()));
        let loader_id = bpf_loader_upgradeable::id();
        let program_address = Pubkey::new_unique();
        let buffer_address = Pubkey::new_unique();
        let spill_address = Pubkey::new_unique();
        let rent = Rent::default();
        let (programdata_address, _) =
            Pubkey::find_program_address(&[program_address.as_ref()], &loader_id);

        let mut buffer_account = AccountSharedData::new(buffer_lamports, buffer_len, &loader_id);
        buffer_account
            .set_state(&UpgradeableLoaderState::Buffer {
                authority_address: Some(*upgrade_authority_address),
            })
            .unwrap();
        let buffer_data_offset = UpgradeableLoaderState::size_of_buffer_metadata();
        buffer_account
            .data_as_mut_slice()
            .get_mut(buffer_data_offset..buffer_data_offset.saturating_add(elf_new.len()))
            .unwrap()
            .copy_from_slice(elf_new);

        let mut programdata_account =
            AccountSharedData::new(programdata_lamports, programdata_len, &loader_id);
        programdata_account
            .set_state(&UpgradeableLoaderState::ProgramData {
                slot: SLOT,
                upgrade_authority_address: Some(*upgrade_authority_address),
            })
            .unwrap();
        let programdata_data_offset = UpgradeableLoaderState::size_of_programdata_metadata();
        programdata_account
            .data_as_mut_slice()
            .get_mut(
                programdata_data_offset..programdata_data_offset.saturating_add(elf_orig.len()),
            )
            .unwrap()
            .copy_from_slice(elf_orig);

        let mut program_account = AccountSharedData::new(
            rent.minimum_balance(UpgradeableLoaderState::size_of_program()),
            UpgradeableLoaderState::size_of_program(),
            &loader_id,
        );
        program_account.set_executable(true);
        program_account
            .set_state(&UpgradeableLoaderState::Program {
                programdata_address,
            })
            .unwrap();

        let spill_account = AccountSharedData::new(0, 0, &Pubkey::new_unique());
        let rent_account = create_sysvar_account(&rent);
        let clock_account = create_sysvar_account(&Clock {
            slot: SLOT.saturating_add(1),
            ..Clock::default()
        });
        let upgrade_authority_account = AccountSharedData::new(1, 0, &Pubkey::new_unique());
        let transaction_accounts = vec![
            (programdata_address, programdata_account),
            (program_address, program_account),
            (buffer_address, buffer_account),
            (spill_address, spill_account),
            (sysvar::rent::id(), rent_account),
            (sysvar::clock::id(), clock_account),
            (*upgrade_authority_address, upgrade_authority_account),
        ];
        let instruction_accounts = vec![
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
                pubkey: buffer_address,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: spill_address,
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
                pubkey: *upgrade_authority_address,
                is_signer: true,
                is_writable: false,
            },
        ];
        (transaction_accounts, instruction_accounts)
    }

    let process_instruction =
        |transaction_accounts: Vec<(Pubkey, AccountSharedData)>,
         instruction_accounts: Vec<AccountMeta>,
         expected_result: Result<(), InstructionError>| {
            let instruction_data =
                bincode::serialize(&UpgradeableLoaderInstruction::Upgrade).unwrap();
            process_instruction_with_setup(
                &bpf_loader_upgradeable::id(),
                &instruction_data,
                transaction_accounts,
                instruction_accounts,
                LoaderV3Features {
                    minimum_extend_program_size: true,
                    set_programdata_to_elf_length: true,
                },
                expected_result,
                |_invoke_context| {},
            )
        };

    let rent = Rent::default();
    let programdata_data_offset = UpgradeableLoaderState::size_of_programdata_metadata();
    let small_len = UpgradeableLoaderState::size_of_programdata(elf_small.len());
    let large_len = UpgradeableLoaderState::size_of_programdata(elf_large.len());
    let small_balance = rent.minimum_balance(small_len);
    let large_balance = rent.minimum_balance(large_len);

    let assert_upgraded = |accounts: &[AccountSharedData], elf_new: &[u8], expected_len: usize| {
        let programdata = accounts.first().unwrap();
        // Programdata has expected length.,
        assert_eq!(expected_len, programdata.data().len());
        // Rent-exempt for its new size.
        assert_eq!(rent.minimum_balance(expected_len), programdata.lamports());
        // ELF is the new ELF.
        assert_eq!(
            elf_new,
            programdata
                .data()
                .get(programdata_data_offset..programdata_data_offset.saturating_add(elf_new.len()))
                .unwrap()
        );
        // Metadata unchanged.
        let state: UpgradeableLoaderState = programdata.state().unwrap();
        assert_eq!(
            UpgradeableLoaderState::ProgramData {
                slot: SLOT.saturating_add(1),
                upgrade_authority_address: Some(upgrade_authority_address),
            },
            state
        );
        // Buffer cleared.
        let buffer = accounts.get(2).unwrap();
        assert_eq!(0, buffer.lamports());
        assert_eq!(
            UpgradeableLoaderState::size_of_buffer(0),
            buffer.data().len()
        );
    };

    // Case: Shrink success
    let (transaction_accounts, instruction_accounts) = get_accounts(
        &upgrade_authority_address,
        &elf_large,
        &elf_small,
        large_len,
        large_balance,
        UpgradeableLoaderState::size_of_buffer(elf_small.len()),
        1,
    );
    let accounts = process_instruction(transaction_accounts, instruction_accounts, Ok(()));
    assert_upgraded(&accounts, &elf_small, small_len);
    assert_eq!(
        large_balance
            .saturating_sub(small_balance)
            .saturating_add(1),
        accounts.get(3).unwrap().lamports()
    );

    // Case: Shrink success overprovisioned programdata
    let extended_len = large_len.saturating_add(4096);
    let extended_balance = rent.minimum_balance(extended_len);
    let (transaction_accounts, instruction_accounts) = get_accounts(
        &upgrade_authority_address,
        &elf_large,
        &elf_small,
        extended_len,
        extended_balance,
        UpgradeableLoaderState::size_of_buffer(elf_small.len()),
        1,
    );
    let accounts = process_instruction(transaction_accounts, instruction_accounts, Ok(()));
    assert_upgraded(&accounts, &elf_small, small_len);
    assert_eq!(
        extended_balance
            .saturating_sub(small_balance)
            .saturating_add(1),
        accounts.get(3).unwrap().lamports()
    );

    // Case: Shrink success larger ELF
    //
    // The new ELF is bigger, but the account was over-provisioned past
    // even that, so it still retracts and still refunds rent.
    let extended_len = large_len.saturating_add(4096);
    let extended_balance = rent.minimum_balance(extended_len);
    let (transaction_accounts, instruction_accounts) = get_accounts(
        &upgrade_authority_address,
        &elf_small,
        &elf_large,
        extended_len,
        extended_balance,
        UpgradeableLoaderState::size_of_buffer(elf_large.len()),
        1,
    );
    let accounts = process_instruction(transaction_accounts, instruction_accounts, Ok(()));
    assert_upgraded(&accounts, &elf_large, large_len);
    assert!(small_len < large_len && large_len < extended_len);
    assert_eq!(
        extended_balance
            .saturating_sub(large_balance)
            .saturating_add(1),
        accounts.get(3).unwrap().lamports()
    );

    // Case: Shrink success overprovisioned buffer
    let padded_buffer_len =
        UpgradeableLoaderState::size_of_buffer(elf_small.len()).saturating_add(32);
    let padded_len = small_len.saturating_add(32);
    let padded_balance = rent.minimum_balance(padded_len);
    assert!(padded_len < large_len);
    let (transaction_accounts, instruction_accounts) = get_accounts(
        &upgrade_authority_address,
        &elf_large,
        &elf_small,
        large_len,
        large_balance,
        padded_buffer_len,
        1,
    );
    let accounts = process_instruction(transaction_accounts, instruction_accounts, Ok(()));
    assert_upgraded(&accounts, &elf_small, padded_len);
    // The padding should still be all zeroes.
    assert!(
        accounts
            .first()
            .unwrap()
            .data()
            .get(programdata_data_offset.saturating_add(elf_small.len())..)
            .unwrap()
            .iter()
            .all(|byte| *byte == 0)
    );
    assert_eq!(
        large_balance
            .saturating_sub(padded_balance)
            .saturating_add(1),
        accounts.get(3).unwrap().lamports()
    );

    // Case: Shrink success funded for the new size only
    let (transaction_accounts, instruction_accounts) = get_accounts(
        &upgrade_authority_address,
        &elf_large,
        &elf_small,
        large_len,
        small_balance, // <-- only enough for the new ELF
        UpgradeableLoaderState::size_of_buffer(elf_small.len()),
        0,
    );
    let accounts = process_instruction(transaction_accounts, instruction_accounts, Ok(()));
    assert_upgraded(&accounts, &elf_small, small_len);
    assert_eq!(0, accounts.get(3).unwrap().lamports());

    // Case: Shrink insufficient funds
    // Same as above, but 1 lamport shy.
    let (transaction_accounts, instruction_accounts) = get_accounts(
        &upgrade_authority_address,
        &elf_large,
        &elf_small,
        large_len,
        small_balance.saturating_sub(1),
        UpgradeableLoaderState::size_of_buffer(elf_small.len()),
        0,
    );
    process_instruction(
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::InsufficientFunds),
    );

    // Case: Grow success
    let (transaction_accounts, instruction_accounts) = get_accounts(
        &upgrade_authority_address,
        &elf_small,
        &elf_large,
        small_len,
        small_balance,
        UpgradeableLoaderState::size_of_buffer(elf_large.len()),
        large_balance,
    );
    let accounts = process_instruction(transaction_accounts, instruction_accounts, Ok(()));
    assert_upgraded(&accounts, &elf_large, large_len);
    // The buffer covered the new rent, so ProgramData's whole original
    // balance spills.
    assert_eq!(small_balance, accounts.get(3).unwrap().lamports());

    // Case: Grow success overprovisioned programdata
    let extended_len = small_len.saturating_add(50);
    let extended_balance = rent.minimum_balance(extended_len);
    assert!(extended_len < large_len);
    let (transaction_accounts, instruction_accounts) = get_accounts(
        &upgrade_authority_address,
        &elf_small,
        &elf_large,
        extended_len,
        extended_balance,
        UpgradeableLoaderState::size_of_buffer(elf_large.len()),
        large_balance,
    );
    let accounts = process_instruction(transaction_accounts, instruction_accounts, Ok(()));
    assert_upgraded(&accounts, &elf_large, large_len);
    // ProgramData lands on the new ELF's length, so the extra bytes are
    // overwritten. Again the buffer covers the rent, so the whole
    // ProgramData balance is swept.
    assert_eq!(extended_balance, accounts.get(3).unwrap().lamports());

    // Case: Grow success overprovisioned buffer
    let padded_buffer_len =
        UpgradeableLoaderState::size_of_buffer(elf_large.len()).saturating_add(64);
    let padded_len = large_len.saturating_add(64);
    let padded_balance = rent.minimum_balance(padded_len);
    let (transaction_accounts, instruction_accounts) = get_accounts(
        &upgrade_authority_address,
        &elf_small,
        &elf_large,
        small_len,
        padded_balance,
        padded_buffer_len,
        0,
    );
    let accounts = process_instruction(transaction_accounts, instruction_accounts, Ok(()));
    assert_upgraded(&accounts, &elf_large, padded_len);
    assert!(
        accounts
            .first()
            .unwrap()
            .data()
            .get(programdata_data_offset.saturating_add(elf_large.len())..)
            .unwrap()
            .iter()
            .all(|byte| *byte == 0)
    );
    assert_eq!(0, accounts.get(3).unwrap().lamports());

    // Case: Grow success funded by programdata
    let (transaction_accounts, instruction_accounts) = get_accounts(
        &upgrade_authority_address,
        &elf_small,
        &elf_large,
        small_len,
        large_balance,
        UpgradeableLoaderState::size_of_buffer(elf_large.len()),
        0,
    );
    let accounts = process_instruction(transaction_accounts, instruction_accounts, Ok(()));
    assert_upgraded(&accounts, &elf_large, large_len);
    // The buffer is empty; ProgramData's own balance covers the new rent.
    assert_eq!(0, accounts.get(3).unwrap().lamports());

    // Case: Grow, insufficient funds
    let deficit = large_balance.saturating_sub(small_balance);
    let (transaction_accounts, instruction_accounts) = get_accounts(
        &upgrade_authority_address,
        &elf_small,
        &elf_large,
        small_len,
        small_balance,
        UpgradeableLoaderState::size_of_buffer(elf_large.len()),
        deficit.saturating_sub(1), // <-- 1 lamport shy
    );
    process_instruction(
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::InsufficientFunds),
    );

    // Case: No resize, ELF length already matches
    let (transaction_accounts, instruction_accounts) = get_accounts(
        &upgrade_authority_address,
        &elf_large,
        &elf_large,
        large_len,
        large_balance,
        UpgradeableLoaderState::size_of_buffer(elf_large.len()),
        1,
    );
    let accounts = process_instruction(transaction_accounts, instruction_accounts, Ok(()));
    assert_upgraded(&accounts, &elf_large, large_len);
    // Just the buffer lamports get swept.
    assert_eq!(1, accounts.get(3).unwrap().lamports());

    // Case: Zero-length ELF in the buffer
    let (transaction_accounts, instruction_accounts) = get_accounts(
        &upgrade_authority_address,
        &elf_large,
        &[],
        large_len,
        large_balance,
        UpgradeableLoaderState::size_of_buffer(0),
        1,
    );
    process_instruction(
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::InvalidAccountData),
    );

    // Case: New length exceeds the max account data length
    let oversized_elf_len = (MAX_PERMITTED_DATA_LENGTH as usize)
        .saturating_sub(UpgradeableLoaderState::size_of_buffer_metadata());
    let mut oversized_elf = elf_large.clone();
    oversized_elf.resize(oversized_elf_len, 0);
    assert!(
        UpgradeableLoaderState::size_of_programdata(oversized_elf.len())
            > MAX_PERMITTED_DATA_LENGTH as usize
    );
    let (transaction_accounts, instruction_accounts) = get_accounts(
        &upgrade_authority_address,
        &elf_small,
        &oversized_elf,
        small_len,
        u64::MAX / 2,
        UpgradeableLoaderState::size_of_buffer(oversized_elf.len()),
        0,
    );
    process_instruction(
        transaction_accounts,
        instruction_accounts,
        Err(InstructionError::InvalidAccountData),
    );
}
