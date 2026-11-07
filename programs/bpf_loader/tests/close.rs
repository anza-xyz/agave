use {
    common::{
        LoaderV3Features, create_sysvar_account, process_instruction_with_conformance_harness,
    },
    solana_account::{
        Account, ReadableAccount, WritableAccount, state_traits::StateMutWincode as StateMut,
    },
    solana_clock::Clock,
    solana_instruction::{AccountMeta, Instruction},
    solana_instruction_error::InstructionError,
    solana_loader_v3_interface::{
        instruction::UpgradeableLoaderInstruction, state::UpgradeableLoaderState,
    },
    solana_program_runtime::execution_budget::DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_sdk_ids::{bpf_loader_upgradeable, system_program, sysvar},
};

mod common;

#[test]
fn test_bpf_loader_upgradeable_close() {
    let instruction = wincode::serialize(&UpgradeableLoaderInstruction::Close).unwrap();
    let loader_id = bpf_loader_upgradeable::id();
    let invalid_authority_address = Pubkey::new_unique();
    let authority_address = Pubkey::new_unique();
    let authority_account = Account::empty(1, &Pubkey::new_unique());
    let recipient_address = Pubkey::new_unique();
    let recipient_account = Account::empty(1, &Pubkey::new_unique());
    let buffer_address = Pubkey::new_unique();
    let buffer_account = Account::new_data_with_space(
        1,
        &UpgradeableLoaderState::Buffer {
            authority_address: Some(authority_address),
        },
        UpgradeableLoaderState::size_of_buffer(128),
        &loader_id,
    )
    .unwrap();
    let uninitialized_address = Pubkey::new_unique();
    let uninitialized_account = Account::new_data_with_space(
        1,
        &UpgradeableLoaderState::Uninitialized,
        UpgradeableLoaderState::size_of_programdata(0),
        &loader_id,
    )
    .unwrap();
    let programdata_address = Pubkey::new_unique();
    let mut programdata_account = Account::new_data_with_space(
        1,
        &UpgradeableLoaderState::ProgramData {
            slot: 0,
            upgrade_authority_address: Some(authority_address),
        },
        UpgradeableLoaderState::size_of_programdata(128),
        &loader_id,
    )
    .unwrap();
    let program_address = Pubkey::new_unique();
    let mut program_account = Account::new_data_with_space(
        1,
        &UpgradeableLoaderState::Program {
            programdata_address,
        },
        UpgradeableLoaderState::size_of_program(),
        &loader_id,
    )
    .unwrap();
    program_account.set_executable(true);
    let clock_account: Account = create_sysvar_account(&Clock {
        slot: 1,
        ..Clock::default()
    })
    .into();
    let transaction_accounts = vec![
        (buffer_address, buffer_account.clone()),
        (recipient_address, recipient_account.clone()),
        (authority_address, authority_account.clone()),
    ];
    let buffer_meta = AccountMeta {
        pubkey: buffer_address,
        is_signer: false,
        is_writable: true,
    };
    let recipient_meta = AccountMeta {
        pubkey: recipient_address,
        is_signer: false,
        is_writable: true,
    };
    let authority_meta = AccountMeta {
        pubkey: authority_address,
        is_signer: true,
        is_writable: false,
    };

    // Case: close a buffer account
    let accounts = process_instruction_with_conformance_harness(
        Instruction::new_with_bytes(
            loader_id,
            &instruction,
            vec![
                buffer_meta.clone(),
                recipient_meta.clone(),
                authority_meta.clone(),
            ],
        ),
        transaction_accounts,
        LoaderV3Features::all_enabled(),
        DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT as u64,
        Ok(()),
    );
    assert_eq!(0, accounts.first().unwrap().1.lamports());
    assert_eq!(2, accounts.get(1).unwrap().1.lamports());
    let state: UpgradeableLoaderState = accounts.first().unwrap().1.state().unwrap();
    assert_eq!(state, UpgradeableLoaderState::Uninitialized);
    assert_eq!(
        UpgradeableLoaderState::size_of_uninitialized(),
        accounts.first().unwrap().1.data().len()
    );

    // Case: close with wrong authority
    process_instruction_with_conformance_harness(
        Instruction::new_with_bytes(
            loader_id,
            &instruction,
            vec![
                buffer_meta,
                recipient_meta.clone(),
                AccountMeta {
                    pubkey: invalid_authority_address,
                    is_signer: true,
                    is_writable: false,
                },
            ],
        ),
        vec![
            (buffer_address, buffer_account.clone()),
            (recipient_address, recipient_account.clone()),
            (invalid_authority_address, authority_account.clone()),
        ],
        LoaderV3Features::all_enabled(),
        DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT as u64,
        Err(InstructionError::IncorrectAuthority),
    );

    // Case: close an uninitialized account
    let accounts = process_instruction_with_conformance_harness(
        Instruction::new_with_bytes(
            loader_id,
            &instruction,
            vec![
                AccountMeta {
                    pubkey: uninitialized_address,
                    is_signer: false,
                    is_writable: true,
                },
                recipient_meta.clone(),
                authority_meta.clone(),
            ],
        ),
        vec![
            (uninitialized_address, uninitialized_account.clone()),
            (recipient_address, recipient_account.clone()),
            (authority_address, authority_account.clone()),
        ],
        LoaderV3Features::all_enabled(),
        DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT as u64,
        Ok(()),
    );
    assert_eq!(0, accounts.first().unwrap().1.lamports());
    assert_eq!(2, accounts.get(1).unwrap().1.lamports());
    let state: UpgradeableLoaderState = accounts.first().unwrap().1.state().unwrap();
    assert_eq!(state, UpgradeableLoaderState::Uninitialized);
    assert_eq!(
        UpgradeableLoaderState::size_of_uninitialized(),
        accounts.first().unwrap().1.data().len()
    );

    // Case: close a program account with a non-writable program account
    process_instruction_with_conformance_harness(
        Instruction::new_with_bytes(
            loader_id,
            &instruction,
            vec![
                AccountMeta {
                    pubkey: programdata_address,
                    is_signer: false,
                    is_writable: true,
                },
                recipient_meta.clone(),
                authority_meta.clone(),
                AccountMeta {
                    pubkey: program_address,
                    is_signer: false,
                    is_writable: false,
                },
            ],
        ),
        vec![
            (programdata_address, programdata_account.clone()),
            (recipient_address, recipient_account.clone()),
            (authority_address, authority_account.clone()),
            (program_address, program_account.clone()),
            (sysvar::clock::id(), clock_account.clone()),
        ],
        LoaderV3Features::all_enabled(),
        DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT as u64,
        Err(InstructionError::InvalidArgument),
    );

    // Case: close a program account
    let accounts = process_instruction_with_conformance_harness(
        Instruction::new_with_bytes(
            loader_id,
            &instruction,
            vec![
                AccountMeta {
                    pubkey: programdata_address,
                    is_signer: false,
                    is_writable: true,
                },
                recipient_meta,
                authority_meta,
                AccountMeta {
                    pubkey: program_address,
                    is_signer: false,
                    is_writable: true,
                },
            ],
        ),
        vec![
            (programdata_address, programdata_account.clone()),
            (recipient_address, recipient_account.clone()),
            (authority_address, authority_account.clone()),
            (program_address, program_account.clone()),
            (sysvar::clock::id(), clock_account.clone()),
        ],
        LoaderV3Features::all_enabled(),
        DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT as u64,
        Ok(()),
    );
    assert_eq!(0, accounts.first().unwrap().1.lamports());
    assert_eq!(2, accounts.get(1).unwrap().1.lamports());
    let state: UpgradeableLoaderState = accounts.first().unwrap().1.state().unwrap();
    assert_eq!(state, UpgradeableLoaderState::Uninitialized);
    assert_eq!(
        UpgradeableLoaderState::size_of_uninitialized(),
        accounts.first().unwrap().1.data().len()
    );

    // Try to invoke closed account
    programdata_account = accounts.first().unwrap().1.clone();
    program_account = accounts.get(3).unwrap().1.clone();
    process_instruction_with_conformance_harness(
        Instruction::new_with_bytes(program_address, &[], Vec::new()),
        vec![
            (programdata_address, programdata_account.clone()),
            (program_address, program_account.clone()),
        ],
        LoaderV3Features::all_enabled(),
        DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT as u64,
        Err(InstructionError::UnsupportedProgramId),
    );

    // Case: Reopen should fail
    process_instruction_with_conformance_harness(
        Instruction::new_with_bytes(
            loader_id,
            &wincode::serialize(&UpgradeableLoaderInstruction::DeployWithMaxDataLen {
                max_data_len: 0,
            })
            .unwrap(),
            vec![
                AccountMeta {
                    pubkey: recipient_address,
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
                    pubkey: buffer_address,
                    is_signer: false,
                    is_writable: false,
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
                    pubkey: authority_address,
                    is_signer: false,
                    is_writable: false,
                },
            ],
        ),
        vec![
            (recipient_address, recipient_account),
            (programdata_address, programdata_account),
            (program_address, program_account),
            (buffer_address, buffer_account),
            (
                sysvar::rent::id(),
                create_sysvar_account(&Rent::default()).into(),
            ),
            (sysvar::clock::id(), clock_account),
            (
                system_program::id(),
                Account::empty(0, &system_program::id()),
            ),
            (authority_address, authority_account),
        ],
        LoaderV3Features::all_enabled(),
        DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT as u64,
        Err(InstructionError::AccountAlreadyInitialized),
    );
}
