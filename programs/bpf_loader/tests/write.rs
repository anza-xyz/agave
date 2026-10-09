use {
    common::process_instruction,
    solana_account::{
        AccountSharedData, ReadableAccount, state_traits::StateMutWincode as StateMut,
    },
    solana_bpf_loader_program::{WRITE_INSTRUCTION_HEADER_LEN, WRITE_INSTRUCTION_TAG},
    solana_instruction::AccountMeta,
    solana_instruction_error::InstructionError,
    solana_loader_v3_interface::{
        instruction::UpgradeableLoaderInstruction, state::UpgradeableLoaderState,
    },
    solana_pubkey::Pubkey,
    solana_sdk_ids::bpf_loader_upgradeable,
};

mod common;

#[test]
fn test_bpf_loader_upgradeable_write() {
    let loader_id = bpf_loader_upgradeable::id();
    let buffer_address = Pubkey::new_unique();
    let mut buffer_account =
        AccountSharedData::new(1, UpgradeableLoaderState::size_of_buffer(9), &loader_id);
    let instruction_accounts = vec![
        AccountMeta {
            pubkey: buffer_address,
            is_signer: false,
            is_writable: true,
        },
        AccountMeta {
            pubkey: buffer_address,
            is_signer: true,
            is_writable: false,
        },
    ];

    // Case: Not initialized
    let instruction = bincode::serialize(&UpgradeableLoaderInstruction::Write {
        offset: 0,
        bytes: vec![42; 9],
    })
    .unwrap();
    process_instruction(
        &loader_id,
        &instruction,
        vec![(buffer_address, buffer_account.clone())],
        instruction_accounts.clone(),
        Err(InstructionError::InvalidAccountData),
    );

    // Case: Write entire buffer
    let instruction = bincode::serialize(&UpgradeableLoaderInstruction::Write {
        offset: 0,
        bytes: vec![42; 9],
    })
    .unwrap();
    buffer_account
        .set_state(&UpgradeableLoaderState::Buffer {
            authority_address: Some(buffer_address),
        })
        .unwrap();
    let accounts = process_instruction(
        &loader_id,
        &instruction,
        vec![(buffer_address, buffer_account.clone())],
        instruction_accounts.clone(),
        Ok(()),
    );
    let state: UpgradeableLoaderState = accounts.first().unwrap().state().unwrap();
    assert_eq!(
        state,
        UpgradeableLoaderState::Buffer {
            authority_address: Some(buffer_address)
        }
    );
    assert_eq!(
        &accounts
            .first()
            .unwrap()
            .data()
            .get(UpgradeableLoaderState::size_of_buffer_metadata()..)
            .unwrap(),
        &[42; 9]
    );

    // Case: Write portion of the buffer
    let instruction = bincode::serialize(&UpgradeableLoaderInstruction::Write {
        offset: 3,
        bytes: vec![42; 6],
    })
    .unwrap();
    let mut buffer_account =
        AccountSharedData::new(1, UpgradeableLoaderState::size_of_buffer(9), &loader_id);
    buffer_account
        .set_state(&UpgradeableLoaderState::Buffer {
            authority_address: Some(buffer_address),
        })
        .unwrap();
    let accounts = process_instruction(
        &loader_id,
        &instruction,
        vec![(buffer_address, buffer_account.clone())],
        instruction_accounts.clone(),
        Ok(()),
    );
    let state: UpgradeableLoaderState = accounts.first().unwrap().state().unwrap();
    assert_eq!(
        state,
        UpgradeableLoaderState::Buffer {
            authority_address: Some(buffer_address)
        }
    );
    assert_eq!(
        &accounts
            .first()
            .unwrap()
            .data()
            .get(UpgradeableLoaderState::size_of_buffer_metadata()..)
            .unwrap(),
        &[0, 0, 0, 42, 42, 42, 42, 42, 42]
    );

    // Case: overflow size
    let instruction = bincode::serialize(&UpgradeableLoaderInstruction::Write {
        offset: 0,
        bytes: vec![42; 10],
    })
    .unwrap();
    buffer_account
        .set_state(&UpgradeableLoaderState::Buffer {
            authority_address: Some(buffer_address),
        })
        .unwrap();
    process_instruction(
        &loader_id,
        &instruction,
        vec![(buffer_address, buffer_account.clone())],
        instruction_accounts.clone(),
        Err(InstructionError::AccountDataTooSmall),
    );

    // Case: overflow offset
    let instruction = bincode::serialize(&UpgradeableLoaderInstruction::Write {
        offset: 1,
        bytes: vec![42; 9],
    })
    .unwrap();
    buffer_account
        .set_state(&UpgradeableLoaderState::Buffer {
            authority_address: Some(buffer_address),
        })
        .unwrap();
    process_instruction(
        &loader_id,
        &instruction,
        vec![(buffer_address, buffer_account.clone())],
        instruction_accounts.clone(),
        Err(InstructionError::AccountDataTooSmall),
    );

    // Case: Not signed
    let instruction = bincode::serialize(&UpgradeableLoaderInstruction::Write {
        offset: 0,
        bytes: vec![42; 9],
    })
    .unwrap();
    buffer_account
        .set_state(&UpgradeableLoaderState::Buffer {
            authority_address: Some(buffer_address),
        })
        .unwrap();
    process_instruction(
        &loader_id,
        &instruction,
        vec![(buffer_address, buffer_account.clone())],
        vec![
            AccountMeta {
                pubkey: buffer_address,
                is_signer: false,
                is_writable: false,
            },
            AccountMeta {
                pubkey: buffer_address,
                is_signer: false,
                is_writable: false,
            },
        ],
        Err(InstructionError::MissingRequiredSignature),
    );

    // Case: wrong authority
    let authority_address = Pubkey::new_unique();
    let instruction = bincode::serialize(&UpgradeableLoaderInstruction::Write {
        offset: 1,
        bytes: vec![42; 9],
    })
    .unwrap();
    buffer_account
        .set_state(&UpgradeableLoaderState::Buffer {
            authority_address: Some(buffer_address),
        })
        .unwrap();
    process_instruction(
        &loader_id,
        &instruction,
        vec![
            (buffer_address, buffer_account.clone()),
            (authority_address, buffer_account.clone()),
        ],
        vec![
            AccountMeta {
                pubkey: buffer_address,
                is_signer: false,
                is_writable: false,
            },
            AccountMeta {
                pubkey: authority_address,
                is_signer: false,
                is_writable: false,
            },
        ],
        Err(InstructionError::IncorrectAuthority),
    );

    // Case: None authority
    let instruction = bincode::serialize(&UpgradeableLoaderInstruction::Write {
        offset: 1,
        bytes: vec![42; 9],
    })
    .unwrap();
    buffer_account
        .set_state(&UpgradeableLoaderState::Buffer {
            authority_address: None,
        })
        .unwrap();
    process_instruction(
        &loader_id,
        &instruction,
        vec![(buffer_address, buffer_account.clone())],
        instruction_accounts,
        Err(InstructionError::Immutable),
    );
}
#[test]
fn test_bpf_loader_upgradeable_write_parsing() {
    let loader_id = bpf_loader_upgradeable::id();
    let buffer_address = Pubkey::new_unique();
    let max_bytes = solana_packet::PACKET_DATA_SIZE - WRITE_INSTRUCTION_HEADER_LEN;
    let mut buffer_account = AccountSharedData::new(
        1,
        UpgradeableLoaderState::size_of_buffer(max_bytes + 1),
        &loader_id,
    );
    buffer_account
        .set_state(&UpgradeableLoaderState::Buffer {
            authority_address: Some(buffer_address),
        })
        .unwrap();
    let instruction_accounts = vec![
        AccountMeta {
            pubkey: buffer_address,
            is_signer: false,
            is_writable: true,
        },
        AccountMeta {
            pubkey: buffer_address,
            is_signer: true,
            is_writable: false,
        },
    ];
    // Hand-assembled `Write`, so the declared length and the bytes actually
    // present can disagree.
    let write = |declared_len: u64, present: usize, trailing: usize| {
        let mut data = WRITE_INSTRUCTION_TAG.to_vec();
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&declared_len.to_le_bytes());
        data.extend(std::iter::repeat_n(42u8, present));
        data.extend(std::iter::repeat_n(7u8, trailing));
        data
    };

    // Case: Trailing bytes are ignored, only the declared payload is written
    let accounts = process_instruction(
        &loader_id,
        &write(9, 9, 100),
        vec![(buffer_address, buffer_account.clone())],
        instruction_accounts.clone(),
        Ok(()),
    );
    let (written, untouched) = accounts
        .first()
        .unwrap()
        .data()
        .get(UpgradeableLoaderState::size_of_buffer_metadata()..)
        .unwrap()
        .split_at(9);
    assert_eq!(written, &[42; 9]);
    assert!(untouched.iter().all(|byte| *byte == 0));

    // Case: Largest payload that fits under the limit
    process_instruction(
        &loader_id,
        &write(max_bytes as u64, max_bytes, 0),
        vec![(buffer_address, buffer_account.clone())],
        instruction_accounts.clone(),
        Ok(()),
    );

    // Case: One byte over the limit
    process_instruction(
        &loader_id,
        &write(max_bytes as u64 + 1, max_bytes + 1, 0),
        vec![(buffer_address, buffer_account.clone())],
        instruction_accounts.clone(),
        Err(InstructionError::InvalidInstructionData),
    );

    // Case: Declared length exceeds the bytes present
    process_instruction(
        &loader_id,
        &write(600, 512, 0),
        vec![(buffer_address, buffer_account.clone())],
        instruction_accounts.clone(),
        Err(InstructionError::InvalidInstructionData),
    );

    // Case: Absurd declared length
    process_instruction(
        &loader_id,
        &write(u64::MAX, 512, 0),
        vec![(buffer_address, buffer_account.clone())],
        instruction_accounts.clone(),
        Err(InstructionError::InvalidInstructionData),
    );

    // Case: Truncated header
    process_instruction(
        &loader_id,
        write(0, 0, 0)
            .get(..WRITE_INSTRUCTION_HEADER_LEN - 1)
            .unwrap(),
        vec![(buffer_address, buffer_account)],
        instruction_accounts,
        Err(InstructionError::InvalidInstructionData),
    );
}
