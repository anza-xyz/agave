use {
    common::{create_sysvar_account, process_instruction},
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
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_sdk_ids::{bpf_loader_upgradeable, system_program, sysvar},
};

mod common;

#[test]
fn test_bpf_loader_upgradeable_close() {
    let instruction = bincode::serialize(&UpgradeableLoaderInstruction::Close).unwrap();
    let loader_id = bpf_loader_upgradeable::id();
    let invalid_authority_address = Pubkey::new_unique();
    let authority_address = Pubkey::new_unique();
    let authority_account = AccountSharedData::new(1, 0, &Pubkey::new_unique());
    let recipient_address = Pubkey::new_unique();
    let recipient_account = AccountSharedData::new(1, 0, &Pubkey::new_unique());
    let buffer_address = Pubkey::new_unique();
    let mut buffer_account =
        AccountSharedData::new(1, UpgradeableLoaderState::size_of_buffer(128), &loader_id);
    buffer_account
        .set_state(&UpgradeableLoaderState::Buffer {
            authority_address: Some(authority_address),
        })
        .unwrap();
    let uninitialized_address = Pubkey::new_unique();
    let mut uninitialized_account = AccountSharedData::new(
        1,
        UpgradeableLoaderState::size_of_programdata(0),
        &loader_id,
    );
    uninitialized_account
        .set_state(&UpgradeableLoaderState::Uninitialized)
        .unwrap();
    let programdata_address = Pubkey::new_unique();
    let mut programdata_account = AccountSharedData::new(
        1,
        UpgradeableLoaderState::size_of_programdata(128),
        &loader_id,
    );
    programdata_account
        .set_state(&UpgradeableLoaderState::ProgramData {
            slot: 0,
            upgrade_authority_address: Some(authority_address),
        })
        .unwrap();
    let program_address = Pubkey::new_unique();
    let mut program_account =
        AccountSharedData::new(1, UpgradeableLoaderState::size_of_program(), &loader_id);
    program_account.set_executable(true);
    program_account
        .set_state(&UpgradeableLoaderState::Program {
            programdata_address,
        })
        .unwrap();
    let clock_account = create_sysvar_account(&Clock {
        slot: 1,
        ..Clock::default()
    });
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
    let accounts = process_instruction(
        &loader_id,
        &instruction,
        transaction_accounts,
        vec![
            buffer_meta.clone(),
            recipient_meta.clone(),
            authority_meta.clone(),
        ],
        Ok(()),
    );
    assert_eq!(0, accounts.first().unwrap().lamports());
    assert_eq!(2, accounts.get(1).unwrap().lamports());
    let state: UpgradeableLoaderState = accounts.first().unwrap().state().unwrap();
    assert_eq!(state, UpgradeableLoaderState::Uninitialized);
    assert_eq!(
        UpgradeableLoaderState::size_of_uninitialized(),
        accounts.first().unwrap().data().len()
    );

    // Case: close with wrong authority
    process_instruction(
        &loader_id,
        &instruction,
        vec![
            (buffer_address, buffer_account.clone()),
            (recipient_address, recipient_account.clone()),
            (invalid_authority_address, authority_account.clone()),
        ],
        vec![
            buffer_meta,
            recipient_meta.clone(),
            AccountMeta {
                pubkey: invalid_authority_address,
                is_signer: true,
                is_writable: false,
            },
        ],
        Err(InstructionError::IncorrectAuthority),
    );

    // Case: close an uninitialized account
    let accounts = process_instruction(
        &loader_id,
        &instruction,
        vec![
            (uninitialized_address, uninitialized_account.clone()),
            (recipient_address, recipient_account.clone()),
            (invalid_authority_address, authority_account.clone()),
        ],
        vec![
            AccountMeta {
                pubkey: uninitialized_address,
                is_signer: false,
                is_writable: true,
            },
            recipient_meta.clone(),
            authority_meta.clone(),
        ],
        Ok(()),
    );
    assert_eq!(0, accounts.first().unwrap().lamports());
    assert_eq!(2, accounts.get(1).unwrap().lamports());
    let state: UpgradeableLoaderState = accounts.first().unwrap().state().unwrap();
    assert_eq!(state, UpgradeableLoaderState::Uninitialized);
    assert_eq!(
        UpgradeableLoaderState::size_of_uninitialized(),
        accounts.first().unwrap().data().len()
    );

    // Case: close a program account with a non-writable program account
    process_instruction(
        &loader_id,
        &instruction,
        vec![
            (programdata_address, programdata_account.clone()),
            (recipient_address, recipient_account.clone()),
            (authority_address, authority_account.clone()),
            (program_address, program_account.clone()),
            (sysvar::clock::id(), clock_account.clone()),
        ],
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
        Err(InstructionError::InvalidArgument),
    );

    // Case: close a program account
    let accounts = process_instruction(
        &loader_id,
        &instruction,
        vec![
            (programdata_address, programdata_account.clone()),
            (recipient_address, recipient_account.clone()),
            (authority_address, authority_account.clone()),
            (program_address, program_account.clone()),
            (sysvar::clock::id(), clock_account.clone()),
        ],
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
        Ok(()),
    );
    assert_eq!(0, accounts.first().unwrap().lamports());
    assert_eq!(2, accounts.get(1).unwrap().lamports());
    let state: UpgradeableLoaderState = accounts.first().unwrap().state().unwrap();
    assert_eq!(state, UpgradeableLoaderState::Uninitialized);
    assert_eq!(
        UpgradeableLoaderState::size_of_uninitialized(),
        accounts.first().unwrap().data().len()
    );

    // Try to invoke closed account
    programdata_account = accounts.first().unwrap().clone();
    program_account = accounts.get(3).unwrap().clone();
    process_instruction(
        &program_address,
        &[],
        vec![
            (programdata_address, programdata_account.clone()),
            (program_address, program_account.clone()),
        ],
        Vec::new(),
        Err(InstructionError::UnsupportedProgramId),
    );

    // Case: Reopen should fail
    process_instruction(
        &loader_id,
        &bincode::serialize(&UpgradeableLoaderInstruction::DeployWithMaxDataLen {
            max_data_len: 0,
        })
        .unwrap(),
        vec![
            (recipient_address, recipient_account),
            (programdata_address, programdata_account),
            (program_address, program_account),
            (buffer_address, buffer_account),
            (sysvar::rent::id(), create_sysvar_account(&Rent::default())),
            (sysvar::clock::id(), clock_account),
            (
                system_program::id(),
                AccountSharedData::new(0, 0, &system_program::id()),
            ),
            (authority_address, authority_account),
        ],
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
        Err(InstructionError::AccountAlreadyInitialized),
    );
}
