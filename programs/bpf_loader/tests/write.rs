use {
    common::{LoaderV3Features, process_instruction, process_instruction_with_setup},
    solana_account::{
        AccountSharedData, ReadableAccount, state_traits::StateMutWincode as StateMut,
    },
    solana_bpf_loader_program::{WRITE_INSTRUCTION_HEADER_LEN, WRITE_INSTRUCTION_TAG, test_utils},
    solana_instruction::AccountMeta,
    solana_instruction_error::InstructionError,
    solana_loader_v3_interface::{
        instruction::UpgradeableLoaderInstruction, state::UpgradeableLoaderState,
    },
    solana_pubkey::Pubkey,
    solana_sdk_ids::bpf_loader_upgradeable,
    test_case::test_case,
};

mod common;

#[test]
fn test_bpf_loader_upgradeable_write() {
    let loader_id = bpf_loader_upgradeable::id();
    let buffer_address = Pubkey::new_unique();
    let mut buffer_account =
        AccountSharedData::new_with_data(1, vec![0; UpgradeableLoaderState::size_of_buffer(9)], &loader_id);
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
    let mut buffer_account = AccountSharedData::new_data_with_space(
        1,
        &UpgradeableLoaderState::Buffer {
            authority_address: Some(buffer_address),
        },
        UpgradeableLoaderState::size_of_buffer(9),
        &loader_id,
    )
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
#[test_case(false; "legacy")]
#[test_case(true; "SIMD-0648")]
fn test_bpf_loader_upgradeable_write_parsing(unbound_loader_v3_instruction_data: bool) {
    let loader_id = bpf_loader_upgradeable::id();
    let features = LoaderV3Features {
        unbound_loader_v3_instruction_data,
        ..LoaderV3Features::all_enabled()
    };
    let buffer_address = Pubkey::new_unique();
    let max_bytes = solana_packet::PACKET_DATA_SIZE - WRITE_INSTRUCTION_HEADER_LEN;
    let buffer_account = AccountSharedData::new_data_with_space(
        1,
        &UpgradeableLoaderState::Buffer {
            authority_address: Some(buffer_address),
        },
        UpgradeableLoaderState::size_of_buffer(max_bytes + 1),
        &loader_id,
    )
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
    let accounts = process_instruction_with_setup(
        &loader_id,
        &write(9, 9, 100),
        vec![(buffer_address, buffer_account.clone())],
        instruction_accounts.clone(),
        features,
        Ok(()),
        |invoke_context| {
            test_utils::load_all_invoked_programs(invoke_context);
        },
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

    // Case: Largest payload that fits under the legacy limit
    process_instruction_with_setup(
        &loader_id,
        &write(max_bytes as u64, max_bytes, 0),
        vec![(buffer_address, buffer_account.clone())],
        instruction_accounts.clone(),
        features,
        Ok(()),
        |invoke_context| {
            test_utils::load_all_invoked_programs(invoke_context);
        },
    );

    // Case: One byte over the legacy limit; passes with SIMD-0648 feature
    process_instruction_with_setup(
        &loader_id,
        &write((max_bytes + 1) as u64, max_bytes + 1, 0),
        vec![(buffer_address, buffer_account.clone())],
        instruction_accounts.clone(),
        features,
        if unbound_loader_v3_instruction_data {
            Ok(())
        } else {
            Err(InstructionError::InvalidInstructionData)
        },
        |invoke_context| {
            test_utils::load_all_invoked_programs(invoke_context);
        },
    );

    // Case: Largest payload the runtime can deliver via CPI; passes with SIMD-0648 feature
    let max_ixn_bytes =
        solana_transaction_context::MAX_INSTRUCTION_DATA_LEN - WRITE_INSTRUCTION_HEADER_LEN;
    let big_buffer_account = AccountSharedData::new_data_with_space(
        1,
        &UpgradeableLoaderState::Buffer {
            authority_address: Some(buffer_address),
        },
        UpgradeableLoaderState::size_of_buffer(max_ixn_bytes),
        &loader_id,
    )
    .unwrap();
    let accounts = process_instruction_with_setup(
        &loader_id,
        &write(max_ixn_bytes as u64, max_ixn_bytes, 0),
        vec![(buffer_address, big_buffer_account)],
        instruction_accounts.clone(),
        features,
        if unbound_loader_v3_instruction_data {
            Ok(())
        } else {
            Err(InstructionError::InvalidInstructionData)
        },
        |invoke_context| {
            test_utils::load_all_invoked_programs(invoke_context);
        },
    );
    if unbound_loader_v3_instruction_data {
        assert_eq!(
            accounts
                .first()
                .unwrap()
                .data()
                .get(UpgradeableLoaderState::size_of_buffer_metadata()..)
                .unwrap(),
            &vec![42u8; max_ixn_bytes]
        );
    }

    // Case: Declared length exceeds the bytes present
    process_instruction_with_setup(
        &loader_id,
        &write(600, 512, 0),
        vec![(buffer_address, buffer_account.clone())],
        instruction_accounts.clone(),
        features,
        Err(InstructionError::InvalidInstructionData),
        |invoke_context| {
            test_utils::load_all_invoked_programs(invoke_context);
        },
    );

    // Case: Absurd declared length
    process_instruction_with_setup(
        &loader_id,
        &write(u64::MAX, 512, 0),
        vec![(buffer_address, buffer_account.clone())],
        instruction_accounts.clone(),
        features,
        Err(InstructionError::InvalidInstructionData),
        |invoke_context| {
            test_utils::load_all_invoked_programs(invoke_context);
        },
    );

    // Case: Truncated header
    process_instruction_with_setup(
        &loader_id,
        write(0, 0, 0)
            .get(..WRITE_INSTRUCTION_HEADER_LEN - 1)
            .unwrap(),
        vec![(buffer_address, buffer_account)],
        instruction_accounts,
        features,
        Err(InstructionError::InvalidInstructionData),
        |invoke_context| {
            test_utils::load_all_invoked_programs(invoke_context);
        },
    );
}
