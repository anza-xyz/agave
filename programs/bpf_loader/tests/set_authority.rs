use {
    common::process_instruction,
    solana_account::{
        AccountSharedData, WritableAccount, state_traits::StateMutWincode as StateMut,
    },
    solana_instruction::AccountMeta,
    solana_instruction_error::InstructionError,
    solana_loader_v3_interface::{
        instruction::UpgradeableLoaderInstruction, state::UpgradeableLoaderState,
    },
    solana_pubkey::Pubkey,
    solana_sdk_ids::bpf_loader_upgradeable,
    std::{fs::File, io::Read},
};

mod common;

#[test]
fn test_bpf_loader_upgradeable_set_upgrade_authority() {
    let instruction = bincode::serialize(&UpgradeableLoaderInstruction::SetAuthority).unwrap();
    let loader_id = bpf_loader_upgradeable::id();
    let slot = 0;
    let upgrade_authority_address = Pubkey::new_unique();
    let upgrade_authority_account = AccountSharedData::new(1, 0, &Pubkey::new_unique());
    let new_upgrade_authority_address = Pubkey::new_unique();
    let new_upgrade_authority_account = AccountSharedData::new(1, 0, &Pubkey::new_unique());
    let program_address = Pubkey::new_unique();
    let (programdata_address, _) =
        Pubkey::find_program_address(&[program_address.as_ref()], &bpf_loader_upgradeable::id());
    let mut programdata_account = AccountSharedData::new(
        1,
        UpgradeableLoaderState::size_of_programdata(0),
        &bpf_loader_upgradeable::id(),
    );
    programdata_account
        .set_state(&UpgradeableLoaderState::ProgramData {
            slot,
            upgrade_authority_address: Some(upgrade_authority_address),
        })
        .unwrap();
    let programdata_meta = AccountMeta {
        pubkey: programdata_address,
        is_signer: false,
        is_writable: true,
    };
    let upgrade_authority_meta = AccountMeta {
        pubkey: upgrade_authority_address,
        is_signer: true,
        is_writable: false,
    };
    let new_upgrade_authority_meta = AccountMeta {
        pubkey: new_upgrade_authority_address,
        is_signer: false,
        is_writable: false,
    };

    // Case: Set to new authority
    let accounts = process_instruction(
        &loader_id,
        &instruction,
        vec![
            (programdata_address, programdata_account.clone()),
            (upgrade_authority_address, upgrade_authority_account.clone()),
            (
                new_upgrade_authority_address,
                new_upgrade_authority_account.clone(),
            ),
        ],
        vec![
            programdata_meta.clone(),
            upgrade_authority_meta.clone(),
            new_upgrade_authority_meta.clone(),
        ],
        Ok(()),
    );
    let state: UpgradeableLoaderState = accounts.first().unwrap().state().unwrap();
    assert_eq!(
        state,
        UpgradeableLoaderState::ProgramData {
            slot,
            upgrade_authority_address: Some(new_upgrade_authority_address),
        }
    );

    // Case: Finalize
    let accounts = process_instruction(
        &loader_id,
        &instruction,
        vec![
            (programdata_address, programdata_account.clone()),
            (upgrade_authority_address, upgrade_authority_account.clone()),
        ],
        vec![programdata_meta.clone(), upgrade_authority_meta.clone()],
        Ok(()),
    );
    let state: UpgradeableLoaderState = accounts.first().unwrap().state().unwrap();
    assert_eq!(
        state,
        UpgradeableLoaderState::ProgramData {
            slot,
            upgrade_authority_address: None,
        }
    );

    // Case: Finalize a SBPFv0 program
    let mut file = File::open("test_elfs/out/sbpfv0_verifier_err.so").expect("file open failed");
    let mut elf = Vec::new();
    file.read_to_end(&mut elf).unwrap();
    programdata_account.resize(UpgradeableLoaderState::size_of_programdata(elf.len()), 0);
    programdata_account
        .data_as_mut_slice()
        .get_mut(UpgradeableLoaderState::size_of_programdata_metadata()..)
        .unwrap()
        .copy_from_slice(&elf);
    process_instruction(
        &loader_id,
        &instruction,
        vec![
            (programdata_address, programdata_account.clone()),
            (upgrade_authority_address, upgrade_authority_account.clone()),
        ],
        vec![programdata_meta.clone(), upgrade_authority_meta.clone()],
        Err(InstructionError::InvalidAccountData),
    );

    // Case: Authority did not sign
    process_instruction(
        &loader_id,
        &instruction,
        vec![
            (programdata_address, programdata_account.clone()),
            (upgrade_authority_address, upgrade_authority_account.clone()),
        ],
        vec![
            programdata_meta.clone(),
            AccountMeta {
                pubkey: upgrade_authority_address,
                is_signer: false,
                is_writable: false,
            },
        ],
        Err(InstructionError::MissingRequiredSignature),
    );

    // Case: wrong authority
    let invalid_upgrade_authority_address = Pubkey::new_unique();
    process_instruction(
        &loader_id,
        &instruction,
        vec![
            (programdata_address, programdata_account.clone()),
            (
                invalid_upgrade_authority_address,
                upgrade_authority_account.clone(),
            ),
            (new_upgrade_authority_address, new_upgrade_authority_account),
        ],
        vec![
            programdata_meta.clone(),
            AccountMeta {
                pubkey: invalid_upgrade_authority_address,
                is_signer: true,
                is_writable: false,
            },
            new_upgrade_authority_meta,
        ],
        Err(InstructionError::IncorrectAuthority),
    );

    // Case: No authority
    programdata_account
        .set_state(&UpgradeableLoaderState::ProgramData {
            slot,
            upgrade_authority_address: None,
        })
        .unwrap();
    process_instruction(
        &loader_id,
        &instruction,
        vec![
            (programdata_address, programdata_account.clone()),
            (upgrade_authority_address, upgrade_authority_account.clone()),
        ],
        vec![programdata_meta.clone(), upgrade_authority_meta.clone()],
        Err(InstructionError::Immutable),
    );

    // Case: Not a ProgramData account
    programdata_account
        .set_state(&UpgradeableLoaderState::Program {
            programdata_address: Pubkey::new_unique(),
        })
        .unwrap();
    process_instruction(
        &loader_id,
        &instruction,
        vec![
            (programdata_address, programdata_account.clone()),
            (upgrade_authority_address, upgrade_authority_account),
        ],
        vec![programdata_meta, upgrade_authority_meta],
        Err(InstructionError::InvalidArgument),
    );
}

#[test]
fn test_bpf_loader_upgradeable_set_upgrade_authority_checked() {
    let instruction =
        bincode::serialize(&UpgradeableLoaderInstruction::SetAuthorityChecked).unwrap();
    let loader_id = bpf_loader_upgradeable::id();
    let slot = 0;
    let upgrade_authority_address = Pubkey::new_unique();
    let upgrade_authority_account = AccountSharedData::new(1, 0, &Pubkey::new_unique());
    let new_upgrade_authority_address = Pubkey::new_unique();
    let new_upgrade_authority_account = AccountSharedData::new(1, 0, &Pubkey::new_unique());
    let program_address = Pubkey::new_unique();
    let (programdata_address, _) =
        Pubkey::find_program_address(&[program_address.as_ref()], &bpf_loader_upgradeable::id());
    let mut programdata_account = AccountSharedData::new(
        1,
        UpgradeableLoaderState::size_of_programdata(0),
        &bpf_loader_upgradeable::id(),
    );
    programdata_account
        .set_state(&UpgradeableLoaderState::ProgramData {
            slot,
            upgrade_authority_address: Some(upgrade_authority_address),
        })
        .unwrap();
    let programdata_meta = AccountMeta {
        pubkey: programdata_address,
        is_signer: false,
        is_writable: true,
    };
    let upgrade_authority_meta = AccountMeta {
        pubkey: upgrade_authority_address,
        is_signer: true,
        is_writable: false,
    };
    let new_upgrade_authority_meta = AccountMeta {
        pubkey: new_upgrade_authority_address,
        is_signer: true,
        is_writable: false,
    };

    // Case: Set to new authority
    let accounts = process_instruction(
        &loader_id,
        &instruction,
        vec![
            (programdata_address, programdata_account.clone()),
            (upgrade_authority_address, upgrade_authority_account.clone()),
            (
                new_upgrade_authority_address,
                new_upgrade_authority_account.clone(),
            ),
        ],
        vec![
            programdata_meta.clone(),
            upgrade_authority_meta.clone(),
            new_upgrade_authority_meta.clone(),
        ],
        Ok(()),
    );

    let state: UpgradeableLoaderState = accounts.first().unwrap().state().unwrap();
    assert_eq!(
        state,
        UpgradeableLoaderState::ProgramData {
            slot,
            upgrade_authority_address: Some(new_upgrade_authority_address),
        }
    );

    // Case: set to same authority
    process_instruction(
        &loader_id,
        &instruction,
        vec![
            (programdata_address, programdata_account.clone()),
            (upgrade_authority_address, upgrade_authority_account.clone()),
        ],
        vec![
            programdata_meta.clone(),
            upgrade_authority_meta.clone(),
            upgrade_authority_meta.clone(),
        ],
        Ok(()),
    );

    // Case: present authority not in instruction
    process_instruction(
        &loader_id,
        &instruction,
        vec![
            (programdata_address, programdata_account.clone()),
            (upgrade_authority_address, upgrade_authority_account.clone()),
            (
                new_upgrade_authority_address,
                new_upgrade_authority_account.clone(),
            ),
        ],
        vec![programdata_meta.clone(), new_upgrade_authority_meta.clone()],
        Err(InstructionError::MissingAccount),
    );

    // Case: new authority not in instruction
    process_instruction(
        &loader_id,
        &instruction,
        vec![
            (programdata_address, programdata_account.clone()),
            (upgrade_authority_address, upgrade_authority_account.clone()),
            (
                new_upgrade_authority_address,
                new_upgrade_authority_account.clone(),
            ),
        ],
        vec![programdata_meta.clone(), upgrade_authority_meta.clone()],
        Err(InstructionError::MissingAccount),
    );

    // Case: present authority did not sign
    process_instruction(
        &loader_id,
        &instruction,
        vec![
            (programdata_address, programdata_account.clone()),
            (upgrade_authority_address, upgrade_authority_account.clone()),
            (
                new_upgrade_authority_address,
                new_upgrade_authority_account.clone(),
            ),
        ],
        vec![
            programdata_meta.clone(),
            AccountMeta {
                pubkey: upgrade_authority_address,
                is_signer: false,
                is_writable: false,
            },
            new_upgrade_authority_meta.clone(),
        ],
        Err(InstructionError::MissingRequiredSignature),
    );

    // Case: New authority did not sign
    process_instruction(
        &loader_id,
        &instruction,
        vec![
            (programdata_address, programdata_account.clone()),
            (upgrade_authority_address, upgrade_authority_account.clone()),
            (
                new_upgrade_authority_address,
                new_upgrade_authority_account.clone(),
            ),
        ],
        vec![
            programdata_meta.clone(),
            upgrade_authority_meta.clone(),
            AccountMeta {
                pubkey: new_upgrade_authority_address,
                is_signer: false,
                is_writable: false,
            },
        ],
        Err(InstructionError::MissingRequiredSignature),
    );

    // Case: wrong present authority
    let invalid_upgrade_authority_address = Pubkey::new_unique();
    process_instruction(
        &loader_id,
        &instruction,
        vec![
            (programdata_address, programdata_account.clone()),
            (
                invalid_upgrade_authority_address,
                upgrade_authority_account.clone(),
            ),
            (new_upgrade_authority_address, new_upgrade_authority_account),
        ],
        vec![
            programdata_meta.clone(),
            AccountMeta {
                pubkey: invalid_upgrade_authority_address,
                is_signer: true,
                is_writable: false,
            },
            new_upgrade_authority_meta.clone(),
        ],
        Err(InstructionError::IncorrectAuthority),
    );

    // Case: programdata is immutable
    programdata_account
        .set_state(&UpgradeableLoaderState::ProgramData {
            slot,
            upgrade_authority_address: None,
        })
        .unwrap();
    process_instruction(
        &loader_id,
        &instruction,
        vec![
            (programdata_address, programdata_account.clone()),
            (upgrade_authority_address, upgrade_authority_account.clone()),
        ],
        vec![
            programdata_meta.clone(),
            upgrade_authority_meta.clone(),
            new_upgrade_authority_meta.clone(),
        ],
        Err(InstructionError::Immutable),
    );

    // Case: Not a ProgramData account
    programdata_account
        .set_state(&UpgradeableLoaderState::Program {
            programdata_address: Pubkey::new_unique(),
        })
        .unwrap();
    process_instruction(
        &loader_id,
        &instruction,
        vec![
            (programdata_address, programdata_account.clone()),
            (upgrade_authority_address, upgrade_authority_account),
        ],
        vec![
            programdata_meta,
            upgrade_authority_meta,
            new_upgrade_authority_meta,
        ],
        Err(InstructionError::InvalidArgument),
    );
}

#[test]
fn test_bpf_loader_upgradeable_set_buffer_authority() {
    let instruction = bincode::serialize(&UpgradeableLoaderInstruction::SetAuthority).unwrap();
    let loader_id = bpf_loader_upgradeable::id();
    let invalid_authority_address = Pubkey::new_unique();
    let authority_address = Pubkey::new_unique();
    let authority_account = AccountSharedData::new(1, 0, &Pubkey::new_unique());
    let new_authority_address = Pubkey::new_unique();
    let new_authority_account = AccountSharedData::new(1, 0, &Pubkey::new_unique());
    let buffer_address = Pubkey::new_unique();
    let mut buffer_account =
        AccountSharedData::new(1, UpgradeableLoaderState::size_of_buffer(0), &loader_id);
    buffer_account
        .set_state(&UpgradeableLoaderState::Buffer {
            authority_address: Some(authority_address),
        })
        .unwrap();
    let mut transaction_accounts = vec![
        (buffer_address, buffer_account.clone()),
        (authority_address, authority_account.clone()),
        (new_authority_address, new_authority_account.clone()),
    ];
    let buffer_meta = AccountMeta {
        pubkey: buffer_address,
        is_signer: false,
        is_writable: true,
    };
    let authority_meta = AccountMeta {
        pubkey: authority_address,
        is_signer: true,
        is_writable: false,
    };
    let new_authority_meta = AccountMeta {
        pubkey: new_authority_address,
        is_signer: false,
        is_writable: false,
    };

    // Case: New authority required
    let accounts = process_instruction(
        &loader_id,
        &instruction,
        transaction_accounts.clone(),
        vec![buffer_meta.clone(), authority_meta.clone()],
        Err(InstructionError::IncorrectAuthority),
    );
    let state: UpgradeableLoaderState = accounts.first().unwrap().state().unwrap();
    assert_eq!(
        state,
        UpgradeableLoaderState::Buffer {
            authority_address: Some(authority_address),
        }
    );

    // Case: Set to new authority
    buffer_account
        .set_state(&UpgradeableLoaderState::Buffer {
            authority_address: Some(authority_address),
        })
        .unwrap();
    let accounts = process_instruction(
        &loader_id,
        &instruction,
        transaction_accounts.clone(),
        vec![
            buffer_meta.clone(),
            authority_meta.clone(),
            new_authority_meta.clone(),
        ],
        Ok(()),
    );
    let state: UpgradeableLoaderState = accounts.first().unwrap().state().unwrap();
    assert_eq!(
        state,
        UpgradeableLoaderState::Buffer {
            authority_address: Some(new_authority_address),
        }
    );

    // Case: Authority did not sign
    process_instruction(
        &loader_id,
        &instruction,
        transaction_accounts.clone(),
        vec![
            buffer_meta.clone(),
            AccountMeta {
                pubkey: authority_address,
                is_signer: false,
                is_writable: false,
            },
            new_authority_meta.clone(),
        ],
        Err(InstructionError::MissingRequiredSignature),
    );

    // Case: wrong authority
    process_instruction(
        &loader_id,
        &instruction,
        vec![
            (buffer_address, buffer_account.clone()),
            (invalid_authority_address, authority_account),
            (new_authority_address, new_authority_account),
        ],
        vec![
            buffer_meta.clone(),
            AccountMeta {
                pubkey: invalid_authority_address,
                is_signer: true,
                is_writable: false,
            },
            new_authority_meta.clone(),
        ],
        Err(InstructionError::IncorrectAuthority),
    );

    // Case: No authority
    process_instruction(
        &loader_id,
        &instruction,
        transaction_accounts.clone(),
        vec![buffer_meta.clone(), authority_meta.clone()],
        Err(InstructionError::IncorrectAuthority),
    );

    // Case: Set to no authority
    transaction_accounts
        .get_mut(0)
        .unwrap()
        .1
        .set_state(&UpgradeableLoaderState::Buffer {
            authority_address: None,
        })
        .unwrap();
    process_instruction(
        &loader_id,
        &instruction,
        transaction_accounts.clone(),
        vec![
            buffer_meta.clone(),
            authority_meta.clone(),
            new_authority_meta.clone(),
        ],
        Err(InstructionError::Immutable),
    );

    // Case: Not a Buffer account
    transaction_accounts
        .get_mut(0)
        .unwrap()
        .1
        .set_state(&UpgradeableLoaderState::Program {
            programdata_address: Pubkey::new_unique(),
        })
        .unwrap();
    process_instruction(
        &loader_id,
        &instruction,
        transaction_accounts.clone(),
        vec![buffer_meta, authority_meta, new_authority_meta],
        Err(InstructionError::InvalidArgument),
    );
}

#[test]
fn test_bpf_loader_upgradeable_set_buffer_authority_checked() {
    let instruction =
        bincode::serialize(&UpgradeableLoaderInstruction::SetAuthorityChecked).unwrap();
    let loader_id = bpf_loader_upgradeable::id();
    let invalid_authority_address = Pubkey::new_unique();
    let authority_address = Pubkey::new_unique();
    let authority_account = AccountSharedData::new(1, 0, &Pubkey::new_unique());
    let new_authority_address = Pubkey::new_unique();
    let new_authority_account = AccountSharedData::new(1, 0, &Pubkey::new_unique());
    let buffer_address = Pubkey::new_unique();
    let mut buffer_account =
        AccountSharedData::new(1, UpgradeableLoaderState::size_of_buffer(0), &loader_id);
    buffer_account
        .set_state(&UpgradeableLoaderState::Buffer {
            authority_address: Some(authority_address),
        })
        .unwrap();
    let mut transaction_accounts = vec![
        (buffer_address, buffer_account.clone()),
        (authority_address, authority_account.clone()),
        (new_authority_address, new_authority_account.clone()),
    ];
    let buffer_meta = AccountMeta {
        pubkey: buffer_address,
        is_signer: false,
        is_writable: true,
    };
    let authority_meta = AccountMeta {
        pubkey: authority_address,
        is_signer: true,
        is_writable: false,
    };
    let new_authority_meta = AccountMeta {
        pubkey: new_authority_address,
        is_signer: true,
        is_writable: false,
    };

    // Case: Set to new authority
    buffer_account
        .set_state(&UpgradeableLoaderState::Buffer {
            authority_address: Some(authority_address),
        })
        .unwrap();
    let accounts = process_instruction(
        &loader_id,
        &instruction,
        transaction_accounts.clone(),
        vec![
            buffer_meta.clone(),
            authority_meta.clone(),
            new_authority_meta.clone(),
        ],
        Ok(()),
    );
    let state: UpgradeableLoaderState = accounts.first().unwrap().state().unwrap();
    assert_eq!(
        state,
        UpgradeableLoaderState::Buffer {
            authority_address: Some(new_authority_address),
        }
    );

    // Case: set to same authority
    process_instruction(
        &loader_id,
        &instruction,
        transaction_accounts.clone(),
        vec![
            buffer_meta.clone(),
            authority_meta.clone(),
            authority_meta.clone(),
        ],
        Ok(()),
    );

    // Case: Missing current authority
    process_instruction(
        &loader_id,
        &instruction,
        transaction_accounts.clone(),
        vec![buffer_meta.clone(), new_authority_meta.clone()],
        Err(InstructionError::MissingAccount),
    );

    // Case: Missing new authority
    process_instruction(
        &loader_id,
        &instruction,
        transaction_accounts.clone(),
        vec![buffer_meta.clone(), authority_meta.clone()],
        Err(InstructionError::MissingAccount),
    );

    // Case: wrong present authority
    process_instruction(
        &loader_id,
        &instruction,
        vec![
            (buffer_address, buffer_account.clone()),
            (invalid_authority_address, authority_account),
            (new_authority_address, new_authority_account),
        ],
        vec![
            buffer_meta.clone(),
            AccountMeta {
                pubkey: invalid_authority_address,
                is_signer: true,
                is_writable: false,
            },
            new_authority_meta.clone(),
        ],
        Err(InstructionError::IncorrectAuthority),
    );

    // Case: present authority did not sign
    process_instruction(
        &loader_id,
        &instruction,
        transaction_accounts.clone(),
        vec![
            buffer_meta.clone(),
            AccountMeta {
                pubkey: authority_address,
                is_signer: false,
                is_writable: false,
            },
            new_authority_meta.clone(),
        ],
        Err(InstructionError::MissingRequiredSignature),
    );

    // Case: new authority did not sign
    process_instruction(
        &loader_id,
        &instruction,
        transaction_accounts.clone(),
        vec![
            buffer_meta.clone(),
            authority_meta.clone(),
            AccountMeta {
                pubkey: new_authority_address,
                is_signer: false,
                is_writable: false,
            },
        ],
        Err(InstructionError::MissingRequiredSignature),
    );

    // Case: Not a Buffer account
    transaction_accounts
        .get_mut(0)
        .unwrap()
        .1
        .set_state(&UpgradeableLoaderState::Program {
            programdata_address: Pubkey::new_unique(),
        })
        .unwrap();
    process_instruction(
        &loader_id,
        &instruction,
        transaction_accounts.clone(),
        vec![
            buffer_meta.clone(),
            authority_meta.clone(),
            new_authority_meta.clone(),
        ],
        Err(InstructionError::InvalidArgument),
    );

    // Case: Buffer is immutable
    transaction_accounts
        .get_mut(0)
        .unwrap()
        .1
        .set_state(&UpgradeableLoaderState::Buffer {
            authority_address: None,
        })
        .unwrap();
    process_instruction(
        &loader_id,
        &instruction,
        transaction_accounts.clone(),
        vec![buffer_meta, authority_meta, new_authority_meta],
        Err(InstructionError::Immutable),
    );
}
