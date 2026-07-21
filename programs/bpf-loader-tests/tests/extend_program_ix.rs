use {
    assert_matches::assert_matches,
    common::{
        LoaderV3Features, add_upgradeable_loader_account, assert_ix_error, setup_test_context,
    },
    solana_account::{AccountSharedData, ReadableAccount, WritableAccount},
    solana_clock::Clock,
    solana_instruction::error::InstructionError,
    solana_keypair::Keypair,
    solana_loader_v3_interface::{
        instruction::{MINIMUM_EXTEND_PROGRAM_BYTES, extend_program},
        state::UpgradeableLoaderState,
    },
    solana_program_test::*,
    solana_pubkey::Pubkey,
    solana_sdk_ids::bpf_loader_upgradeable::id,
    solana_signer::Signer,
    solana_system_interface::{
        MAX_PERMITTED_DATA_LENGTH, error::SystemError, instruction as system_instruction,
        program as system_program,
    },
    solana_transaction::Transaction,
    solana_transaction_error::TransactionError,
};

mod common;

#[tokio::test]
async fn test_extend_program() {
    let mut context = setup_test_context(LoaderV3Features {
        minimum_extend_program_size: false,
    })
    .await;
    let program_file = find_file("noop.so").expect("Failed to find the file");
    let data = read_file(program_file);
    let upgrade_authority = Keypair::new();

    let program_address = Pubkey::new_unique();
    let (programdata_address, _) = Pubkey::find_program_address(&[program_address.as_ref()], &id());
    add_upgradeable_loader_account(
        &mut context,
        &program_address,
        &UpgradeableLoaderState::Program {
            programdata_address,
        },
        UpgradeableLoaderState::size_of_program(),
        |_| {},
    )
    .await;
    let programdata_data_offset = UpgradeableLoaderState::size_of_programdata_metadata();
    let program_data_len = data.len() + programdata_data_offset;
    add_upgradeable_loader_account(
        &mut context,
        &programdata_address,
        &UpgradeableLoaderState::ProgramData {
            slot: 0,
            upgrade_authority_address: Some(upgrade_authority.pubkey()),
        },
        program_data_len,
        |account| account.data_as_mut_slice()[programdata_data_offset..].copy_from_slice(&data),
    )
    .await;

    let client = &mut context.banks_client;
    let payer = &context.payer;
    let recent_blockhash = context.last_blockhash;
    const ADDITIONAL_BYTES: u32 = 42;
    let transaction = Transaction::new_signed_with_payer(
        &[extend_program(
            &program_address,
            Some(&payer.pubkey()),
            ADDITIONAL_BYTES,
        )],
        Some(&payer.pubkey()),
        &[payer],
        recent_blockhash,
    );

    assert_matches!(client.process_transaction(transaction).await, Ok(()));
    let updated_program_data_account = client
        .get_account(programdata_address)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        updated_program_data_account.data().len(),
        program_data_len + ADDITIONAL_BYTES as usize
    );
}

#[tokio::test]
async fn test_failed_extend_twice_in_same_slot() {
    let mut context = setup_test_context(LoaderV3Features {
        minimum_extend_program_size: false,
    })
    .await;
    let program_file = find_file("noop.so").expect("Failed to find the file");
    let data = read_file(program_file);
    let upgrade_authority = Keypair::new();

    let program_address = Pubkey::new_unique();
    let (programdata_address, _) = Pubkey::find_program_address(&[program_address.as_ref()], &id());
    add_upgradeable_loader_account(
        &mut context,
        &program_address,
        &UpgradeableLoaderState::Program {
            programdata_address,
        },
        UpgradeableLoaderState::size_of_program(),
        |_| {},
    )
    .await;
    let programdata_data_offset = UpgradeableLoaderState::size_of_programdata_metadata();
    let program_data_len = data.len() + programdata_data_offset;
    add_upgradeable_loader_account(
        &mut context,
        &programdata_address,
        &UpgradeableLoaderState::ProgramData {
            slot: 0,
            upgrade_authority_address: Some(upgrade_authority.pubkey()),
        },
        program_data_len,
        |account| account.data_as_mut_slice()[programdata_data_offset..].copy_from_slice(&data),
    )
    .await;

    let client = &mut context.banks_client;
    let payer = &context.payer;
    let recent_blockhash = context.last_blockhash;
    const ADDITIONAL_BYTES: u32 = 42;
    let transaction = Transaction::new_signed_with_payer(
        &[extend_program(
            &program_address,
            Some(&payer.pubkey()),
            ADDITIONAL_BYTES,
        )],
        Some(&payer.pubkey()),
        &[payer],
        recent_blockhash,
    );

    assert_matches!(client.process_transaction(transaction).await, Ok(()));
    let updated_program_data_account = client
        .get_account(programdata_address)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        updated_program_data_account.data().len(),
        program_data_len + ADDITIONAL_BYTES as usize
    );

    let recent_blockhash = client
        .get_new_latest_blockhash(&recent_blockhash)
        .await
        .unwrap();
    // Extending the program in the same slot should fail
    let transaction = Transaction::new_signed_with_payer(
        &[extend_program(
            &program_address,
            Some(&payer.pubkey()),
            ADDITIONAL_BYTES,
        )],
        Some(&payer.pubkey()),
        &[payer],
        recent_blockhash,
    );

    assert_matches!(
        client
            .process_transaction(transaction)
            .await
            .unwrap_err()
            .unwrap(),
        TransactionError::InstructionError(0, InstructionError::InvalidArgument)
    );
}

#[tokio::test]
async fn test_extend_program_not_upgradeable() {
    let mut context = setup_test_context(LoaderV3Features {
        minimum_extend_program_size: false,
    })
    .await;

    let program_address = Pubkey::new_unique();
    let (programdata_address, _) = Pubkey::find_program_address(&[program_address.as_ref()], &id());
    add_upgradeable_loader_account(
        &mut context,
        &program_address,
        &UpgradeableLoaderState::Program {
            programdata_address,
        },
        UpgradeableLoaderState::size_of_program(),
        |_| {},
    )
    .await;
    add_upgradeable_loader_account(
        &mut context,
        &programdata_address,
        &UpgradeableLoaderState::ProgramData {
            slot: 0,
            upgrade_authority_address: None,
        },
        100,
        |_| {},
    )
    .await;

    let payer_address = context.payer.pubkey();
    assert_ix_error(
        &mut context,
        extend_program(&program_address, Some(&payer_address), 42),
        None,
        InstructionError::Immutable,
        "should fail because the program data account isn't upgradeable",
    )
    .await;
}

#[tokio::test]
async fn test_extend_program_by_zero_bytes() {
    let mut context = setup_test_context(LoaderV3Features {
        minimum_extend_program_size: false,
    })
    .await;
    let upgrade_authority = Keypair::new();

    let program_address = Pubkey::new_unique();
    let (programdata_address, _) = Pubkey::find_program_address(&[program_address.as_ref()], &id());
    add_upgradeable_loader_account(
        &mut context,
        &program_address,
        &UpgradeableLoaderState::Program {
            programdata_address,
        },
        UpgradeableLoaderState::size_of_program(),
        |_| {},
    )
    .await;
    add_upgradeable_loader_account(
        &mut context,
        &programdata_address,
        &UpgradeableLoaderState::ProgramData {
            slot: 0,
            upgrade_authority_address: Some(upgrade_authority.pubkey()),
        },
        100,
        |_| {},
    )
    .await;

    let payer_address = context.payer.pubkey();
    assert_ix_error(
        &mut context,
        extend_program(&program_address, Some(&payer_address), 0),
        None,
        InstructionError::InvalidInstructionData,
        "should fail because the program data account must be extended by more than 0 bytes",
    )
    .await;
}

#[tokio::test]
async fn test_extend_program_past_max_size() {
    let mut context = setup_test_context(LoaderV3Features {
        minimum_extend_program_size: false,
    })
    .await;
    let upgrade_authority = Keypair::new();

    let program_address = Pubkey::new_unique();
    let (programdata_address, _) = Pubkey::find_program_address(&[program_address.as_ref()], &id());
    add_upgradeable_loader_account(
        &mut context,
        &program_address,
        &UpgradeableLoaderState::Program {
            programdata_address,
        },
        UpgradeableLoaderState::size_of_program(),
        |_| {},
    )
    .await;
    add_upgradeable_loader_account(
        &mut context,
        &programdata_address,
        &UpgradeableLoaderState::ProgramData {
            slot: 0,
            upgrade_authority_address: Some(upgrade_authority.pubkey()),
        },
        MAX_PERMITTED_DATA_LENGTH as usize,
        |_| {},
    )
    .await;

    let payer_address = context.payer.pubkey();
    assert_ix_error(
        &mut context,
        extend_program(&program_address, Some(&payer_address), 1),
        None,
        InstructionError::InvalidRealloc,
        "should fail because the program data account cannot be extended past the max data size",
    )
    .await;
}

#[tokio::test]
async fn test_extend_program_with_invalid_payer() {
    let mut context = setup_test_context(LoaderV3Features {
        minimum_extend_program_size: false,
    })
    .await;
    let rent = context.banks_client.get_rent().await.unwrap();
    let upgrade_authority_address = context.payer.pubkey();

    let program_address = Pubkey::new_unique();
    let (programdata_address, _) = Pubkey::find_program_address(&[program_address.as_ref()], &id());
    add_upgradeable_loader_account(
        &mut context,
        &program_address,
        &UpgradeableLoaderState::Program {
            programdata_address,
        },
        UpgradeableLoaderState::size_of_program(),
        |_| {},
    )
    .await;
    add_upgradeable_loader_account(
        &mut context,
        &programdata_address,
        &UpgradeableLoaderState::ProgramData {
            slot: 0,
            upgrade_authority_address: Some(upgrade_authority_address),
        },
        100,
        |_| {},
    )
    .await;

    let payer_with_sufficient_funds = Keypair::new();
    context.set_account(
        &payer_with_sufficient_funds.pubkey(),
        &AccountSharedData::new(10_000_000_000, 0, &system_program::id()),
    );

    let payer_with_insufficient_funds = Keypair::new();
    context.set_account(
        &payer_with_insufficient_funds.pubkey(),
        &AccountSharedData::new(rent.minimum_balance(0), 0, &system_program::id()),
    );

    let payer_with_invalid_owner = Keypair::new();
    context.set_account(
        &payer_with_invalid_owner.pubkey(),
        &AccountSharedData::new(rent.minimum_balance(0), 0, &id()),
    );

    assert_ix_error(
        &mut context,
        extend_program(
            &program_address,
            Some(&payer_with_insufficient_funds.pubkey()),
            1024,
        ),
        Some(&payer_with_insufficient_funds),
        InstructionError::from(SystemError::ResultWithNegativeLamports),
        "should fail because the payer has insufficient funds to cover program data account rent",
    )
    .await;

    assert_ix_error(
        &mut context,
        extend_program(
            &program_address,
            Some(&payer_with_invalid_owner.pubkey()),
            1,
        ),
        Some(&payer_with_invalid_owner),
        InstructionError::ExternalAccountLamportSpend,
        "should fail because the payer is not a system account",
    )
    .await;

    let mut ix = extend_program(
        &program_address,
        Some(&payer_with_sufficient_funds.pubkey()),
        1,
    );

    // Demote payer account meta to non-signer so that transaction signing succeeds
    {
        let payer_meta = ix
            .accounts
            .iter_mut()
            .find(|meta| meta.pubkey == payer_with_sufficient_funds.pubkey())
            .expect("expected to find payer account meta");
        payer_meta.is_signer = false;
    }

    assert_ix_error(
        &mut context,
        ix,
        None,
        InstructionError::PrivilegeEscalation,
        "should fail because the payer did not sign",
    )
    .await;
}

#[tokio::test]
async fn test_extend_program_without_payer() {
    let mut context = setup_test_context(LoaderV3Features {
        minimum_extend_program_size: false,
    })
    .await;
    let rent = context.banks_client.get_rent().await.unwrap();

    let program_file = find_file("noop.so").expect("Failed to find the file");
    let data = read_file(program_file);
    let upgrade_authority = Keypair::new();

    let program_address = Pubkey::new_unique();
    let (programdata_address, _) = Pubkey::find_program_address(&[program_address.as_ref()], &id());
    add_upgradeable_loader_account(
        &mut context,
        &program_address,
        &UpgradeableLoaderState::Program {
            programdata_address,
        },
        UpgradeableLoaderState::size_of_program(),
        |_| {},
    )
    .await;
    let programdata_data_offset = UpgradeableLoaderState::size_of_programdata_metadata();
    let program_data_len = data.len() + programdata_data_offset;
    add_upgradeable_loader_account(
        &mut context,
        &programdata_address,
        &UpgradeableLoaderState::ProgramData {
            slot: 0,
            upgrade_authority_address: Some(upgrade_authority.pubkey()),
        },
        program_data_len,
        |account| account.data_as_mut_slice()[programdata_data_offset..].copy_from_slice(&data),
    )
    .await;

    assert_ix_error(
        &mut context,
        extend_program(&program_address, None, 1024),
        None,
        InstructionError::MissingAccount,
        "should fail because program data has insufficient funds to cover rent",
    )
    .await;

    let client = &mut context.banks_client;
    let payer = &context.payer;
    let recent_blockhash = context.last_blockhash;

    const ADDITIONAL_BYTES: u32 = 42;
    let min_balance_increase_for_extend = rent
        .minimum_balance(ADDITIONAL_BYTES as usize)
        .saturating_sub(rent.minimum_balance(0));

    let transaction = Transaction::new_signed_with_payer(
        &[
            system_instruction::transfer(
                &payer.pubkey(),
                &programdata_address,
                min_balance_increase_for_extend,
            ),
            extend_program(&program_address, None, ADDITIONAL_BYTES),
        ],
        Some(&payer.pubkey()),
        &[payer],
        recent_blockhash,
    );

    assert_matches!(client.process_transaction(transaction).await, Ok(()));
    let updated_program_data_account = client
        .get_account(programdata_address)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        updated_program_data_account.data().len(),
        program_data_len + ADDITIONAL_BYTES as usize
    );
}

#[tokio::test]
async fn test_extend_program_with_invalid_system_program() {
    let mut context = setup_test_context(LoaderV3Features {
        minimum_extend_program_size: false,
    })
    .await;
    let upgrade_authority = Keypair::new();

    let program_address = Pubkey::new_unique();
    let (programdata_address, _) = Pubkey::find_program_address(&[program_address.as_ref()], &id());
    add_upgradeable_loader_account(
        &mut context,
        &program_address,
        &UpgradeableLoaderState::Program {
            programdata_address,
        },
        UpgradeableLoaderState::size_of_program(),
        |_| {},
    )
    .await;
    let program_data_len = 100;
    add_upgradeable_loader_account(
        &mut context,
        &programdata_address,
        &UpgradeableLoaderState::ProgramData {
            slot: 0,
            upgrade_authority_address: Some(upgrade_authority.pubkey()),
        },
        program_data_len,
        |_| {},
    )
    .await;

    let payer_address = context.payer.pubkey();
    let mut ix = extend_program(&program_address, Some(&payer_address), 1);

    // Change system program to an invalid key
    {
        let system_program_meta = ix
            .accounts
            .iter_mut()
            .find(|meta| meta.pubkey == crate::system_program::ID)
            .expect("expected to find system program account meta");
        system_program_meta.pubkey = Pubkey::new_unique();
    }

    assert_ix_error(
        &mut context,
        ix,
        None,
        InstructionError::MissingAccount,
        "should fail because the system program is missing",
    )
    .await;
}

#[tokio::test]
async fn test_extend_program_with_mismatch_program_data() {
    let mut context = setup_test_context(LoaderV3Features {
        minimum_extend_program_size: false,
    })
    .await;
    let payer_address = context.payer.pubkey();
    let upgrade_authority = Keypair::new();

    let program_address = Pubkey::new_unique();
    let (programdata_address, _) = Pubkey::find_program_address(&[program_address.as_ref()], &id());
    add_upgradeable_loader_account(
        &mut context,
        &program_address,
        &UpgradeableLoaderState::Program {
            programdata_address,
        },
        UpgradeableLoaderState::size_of_program(),
        |_| {},
    )
    .await;

    let mismatch_programdata_address = Pubkey::new_unique();
    add_upgradeable_loader_account(
        &mut context,
        &mismatch_programdata_address,
        &UpgradeableLoaderState::ProgramData {
            slot: 0,
            upgrade_authority_address: Some(upgrade_authority.pubkey()),
        },
        100,
        |_| {},
    )
    .await;

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

    assert_ix_error(
        &mut context,
        ix,
        None,
        InstructionError::InvalidArgument,
        "should fail because the program data account doesn't match the program",
    )
    .await;
}

#[tokio::test]
async fn test_extend_program_with_readonly_program_data() {
    let mut context = setup_test_context(LoaderV3Features {
        minimum_extend_program_size: false,
    })
    .await;
    let payer_address = context.payer.pubkey();
    let upgrade_authority = Keypair::new();

    let program_address = Pubkey::new_unique();
    let (programdata_address, _) = Pubkey::find_program_address(&[program_address.as_ref()], &id());
    add_upgradeable_loader_account(
        &mut context,
        &program_address,
        &UpgradeableLoaderState::Program {
            programdata_address,
        },
        UpgradeableLoaderState::size_of_program(),
        |_| {},
    )
    .await;
    add_upgradeable_loader_account(
        &mut context,
        &programdata_address,
        &UpgradeableLoaderState::ProgramData {
            slot: 0,
            upgrade_authority_address: Some(upgrade_authority.pubkey()),
        },
        100,
        |_| {},
    )
    .await;

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

    assert_ix_error(
        &mut context,
        ix,
        None,
        InstructionError::InvalidArgument,
        "should fail because the program data account is not writable",
    )
    .await;
}

#[tokio::test]
async fn test_extend_program_with_invalid_program_data_state() {
    let mut context = setup_test_context(LoaderV3Features {
        minimum_extend_program_size: false,
    })
    .await;
    let payer_address = context.payer.pubkey();

    let program_address = Pubkey::new_unique();
    let (programdata_address, _) = Pubkey::find_program_address(&[program_address.as_ref()], &id());
    add_upgradeable_loader_account(
        &mut context,
        &program_address,
        &UpgradeableLoaderState::Program {
            programdata_address,
        },
        UpgradeableLoaderState::size_of_program(),
        |_| {},
    )
    .await;
    add_upgradeable_loader_account(
        &mut context,
        &programdata_address,
        &UpgradeableLoaderState::Buffer {
            authority_address: Some(payer_address),
        },
        100,
        |_| {},
    )
    .await;

    assert_ix_error(
        &mut context,
        extend_program(&program_address, Some(&payer_address), 1024),
        None,
        InstructionError::InvalidAccountData,
        "should fail because the program data account state isn't valid",
    )
    .await;
}

#[tokio::test]
async fn test_extend_program_with_invalid_program_data_owner() {
    let mut context = setup_test_context(LoaderV3Features {
        minimum_extend_program_size: false,
    })
    .await;
    let payer_address = context.payer.pubkey();

    let program_address = Pubkey::new_unique();
    let (programdata_address, _) = Pubkey::find_program_address(&[program_address.as_ref()], &id());
    add_upgradeable_loader_account(
        &mut context,
        &program_address,
        &UpgradeableLoaderState::Program {
            programdata_address,
        },
        UpgradeableLoaderState::size_of_program(),
        |_| {},
    )
    .await;

    let invalid_owner = Pubkey::new_unique();
    add_upgradeable_loader_account(
        &mut context,
        &program_address,
        &UpgradeableLoaderState::ProgramData {
            slot: 0,
            upgrade_authority_address: Some(payer_address),
        },
        100,
        |account| account.set_owner(invalid_owner),
    )
    .await;

    assert_ix_error(
        &mut context,
        extend_program(&program_address, Some(&payer_address), 1024),
        None,
        InstructionError::InvalidAccountOwner,
        "should fail because the program data account owner isn't valid",
    )
    .await;
}

#[tokio::test]
async fn test_extend_program_with_readonly_program() {
    let mut context = setup_test_context(LoaderV3Features {
        minimum_extend_program_size: false,
    })
    .await;
    let payer_address = context.payer.pubkey();
    let upgrade_authority = Keypair::new();

    let program_address = Pubkey::new_unique();
    let (programdata_address, _) = Pubkey::find_program_address(&[program_address.as_ref()], &id());
    add_upgradeable_loader_account(
        &mut context,
        &program_address,
        &UpgradeableLoaderState::Program {
            programdata_address,
        },
        UpgradeableLoaderState::size_of_program(),
        |_| {},
    )
    .await;
    add_upgradeable_loader_account(
        &mut context,
        &programdata_address,
        &UpgradeableLoaderState::ProgramData {
            slot: 0,
            upgrade_authority_address: Some(upgrade_authority.pubkey()),
        },
        100,
        |_| {},
    )
    .await;

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

    assert_ix_error(
        &mut context,
        ix,
        None,
        InstructionError::InvalidArgument,
        "should fail because the program account is not writable",
    )
    .await;
}

#[tokio::test]
async fn test_extend_program_with_invalid_program_owner() {
    let mut context = setup_test_context(LoaderV3Features {
        minimum_extend_program_size: false,
    })
    .await;
    let payer_address = context.payer.pubkey();
    let upgrade_authority = Keypair::new();

    let program_address = Pubkey::new_unique();
    let (programdata_address, _) = Pubkey::find_program_address(&[program_address.as_ref()], &id());
    let invalid_owner = Pubkey::new_unique();
    add_upgradeable_loader_account(
        &mut context,
        &program_address,
        &UpgradeableLoaderState::Program {
            programdata_address,
        },
        UpgradeableLoaderState::size_of_program(),
        |account| account.set_owner(invalid_owner),
    )
    .await;
    add_upgradeable_loader_account(
        &mut context,
        &programdata_address,
        &UpgradeableLoaderState::ProgramData {
            slot: 0,
            upgrade_authority_address: Some(upgrade_authority.pubkey()),
        },
        100,
        |_| {},
    )
    .await;

    assert_ix_error(
        &mut context,
        extend_program(&program_address, Some(&payer_address), 1024),
        None,
        InstructionError::InvalidAccountOwner,
        "should fail because the program account owner isn't valid",
    )
    .await;
}

#[tokio::test]
async fn test_extend_program_with_invalid_program_state() {
    let mut context = setup_test_context(LoaderV3Features {
        minimum_extend_program_size: false,
    })
    .await;
    let payer_address = context.payer.pubkey();
    let upgrade_authority = Keypair::new();

    let program_address = Pubkey::new_unique();
    let (programdata_address, _) = Pubkey::find_program_address(&[program_address.as_ref()], &id());
    add_upgradeable_loader_account(
        &mut context,
        &program_address,
        &UpgradeableLoaderState::Buffer {
            authority_address: Some(payer_address),
        },
        100,
        |_| {},
    )
    .await;

    add_upgradeable_loader_account(
        &mut context,
        &programdata_address,
        &UpgradeableLoaderState::ProgramData {
            slot: 0,
            upgrade_authority_address: Some(upgrade_authority.pubkey()),
        },
        100,
        |_| {},
    )
    .await;

    assert_ix_error(
        &mut context,
        extend_program(&program_address, Some(&payer_address), 1024),
        None,
        InstructionError::InvalidAccountData,
        "should fail because the program account state isn't valid",
    )
    .await;
}

async fn setup_test_context_for_simd_0431_tests(
    program_address: &Pubkey,
    upgrade_authority_address: &Pubkey,
    programdata_len: usize,
) -> ProgramTestContext {
    // First set up the context with SIMD-0431 ENABLED.
    let mut context = setup_test_context(LoaderV3Features {
        minimum_extend_program_size: true,
    })
    .await;
    let program_file = find_file("noop.so").expect("Failed to find the file");
    let data = read_file(program_file);

    // Set up Program state.
    let (programdata_address, _) = Pubkey::find_program_address(&[program_address.as_ref()], &id());
    add_upgradeable_loader_account(
        &mut context,
        program_address,
        &UpgradeableLoaderState::Program {
            programdata_address,
        },
        UpgradeableLoaderState::size_of_program(),
        |_| {},
    )
    .await;

    // Set up ProgramData state.
    let programdata_data_offset = UpgradeableLoaderState::size_of_programdata_metadata();
    add_upgradeable_loader_account(
        &mut context,
        &programdata_address,
        &UpgradeableLoaderState::ProgramData {
            slot: 0,
            upgrade_authority_address: Some(*upgrade_authority_address),
        },
        programdata_len,
        |account| {
            let end = programdata_data_offset.saturating_add(data.len());
            account.data_as_mut_slice()[programdata_data_offset..end].copy_from_slice(&data)
        },
    )
    .await;

    context
}

#[tokio::test]
async fn test_extend_program_minimum_size_requirement() {
    let program_address = Pubkey::new_unique();
    let upgrade_authority = Keypair::new();
    let starting_programdata_len = (MINIMUM_EXTEND_PROGRAM_BYTES * 4) as usize;

    let mut context = setup_test_context_for_simd_0431_tests(
        &program_address,
        &upgrade_authority.pubkey(),
        starting_programdata_len,
    )
    .await;

    // Anything below the minimum size requirement should fail.
    for additional_bytes in [1, 69, 420, 10_000, MINIMUM_EXTEND_PROGRAM_BYTES - 1] {
        let payer_address = context.payer.pubkey();
        assert_ix_error(
            &mut context,
            extend_program(&program_address, Some(&payer_address), additional_bytes),
            None,
            InstructionError::InvalidArgument,
            "should fail because the requested extension is below the minimum",
        )
        .await;
    }

    // Anything at or above the minimum size requirement should succeed.
    let mut programdata_len = starting_programdata_len;
    let (programdata_address, _) = Pubkey::find_program_address(&[program_address.as_ref()], &id());
    for additional_bytes in [
        MINIMUM_EXTEND_PROGRAM_BYTES,
        MINIMUM_EXTEND_PROGRAM_BYTES + 1,
    ] {
        let client = &mut context.banks_client;
        let payer = &context.payer;
        let recent_blockhash = context.last_blockhash;
        let transaction = Transaction::new_signed_with_payer(
            &[extend_program(
                &program_address,
                Some(&payer.pubkey()),
                additional_bytes,
            )],
            Some(&payer.pubkey()),
            &[payer],
            recent_blockhash,
        );

        assert_matches!(client.process_transaction(transaction).await, Ok(()));
        let updated_program_data_account = client
            .get_account(programdata_address)
            .await
            .unwrap()
            .unwrap();

        let expected_new_len = programdata_len + (additional_bytes as usize);
        assert_eq!(updated_program_data_account.data().len(), expected_new_len,);
        programdata_len = expected_new_len;

        let clock = client.get_sysvar::<Clock>().await.unwrap();
        context.warp_to_slot(clock.slot + 1).unwrap();
    }
}

#[tokio::test]
async fn test_extend_program_minimum_size_requirement_at_matching_headroom() {
    // Set the programdata length so that the headroom is exactly
    // MAX_PERMITTED_DATA_LENGTH - MINIMUM_EXTEND_PROGRAM_BYTES and ensure the
    // minimum requirement applies.

    let program_address = Pubkey::new_unique();
    let upgrade_authority = Keypair::new();
    let programdata_len =
        (MAX_PERMITTED_DATA_LENGTH as usize) - (MINIMUM_EXTEND_PROGRAM_BYTES as usize);

    let mut context = setup_test_context_for_simd_0431_tests(
        &program_address,
        &upgrade_authority.pubkey(),
        programdata_len,
    )
    .await;

    // Anything below the minimum size requirement should fail.
    for additional_bytes in [1, 69, 420, 10_000, MINIMUM_EXTEND_PROGRAM_BYTES - 1] {
        let payer_address = context.payer.pubkey();
        assert_ix_error(
            &mut context,
            extend_program(&program_address, Some(&payer_address), additional_bytes),
            None,
            InstructionError::InvalidArgument,
            "should fail because the requested extension is below the minimum",
        )
        .await;
    }

    // Only exactly MINIMUM_EXTEND_PROGRAM_BYTES succeeds.
    {
        let client = &mut context.banks_client;
        let payer = &context.payer;
        let recent_blockhash = context.last_blockhash;
        let transaction = Transaction::new_signed_with_payer(
            &[extend_program(
                &program_address,
                Some(&payer.pubkey()),
                MINIMUM_EXTEND_PROGRAM_BYTES,
            )],
            Some(&payer.pubkey()),
            &[payer],
            recent_blockhash,
        );

        let (programdata_address, _) =
            Pubkey::find_program_address(&[program_address.as_ref()], &id());

        assert_matches!(client.process_transaction(transaction).await, Ok(()));
        let updated_program_data_account = client
            .get_account(programdata_address)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            updated_program_data_account.data().len(),
            programdata_len + (MINIMUM_EXTEND_PROGRAM_BYTES as usize),
        );
    }
}

#[tokio::test]
async fn test_extend_program_near_max_headroom_requirement() {
    // Set the programdata length so that the headroom is less than
    // MINIMUM_EXTEND_PROGRAM_BYTES and ensure the *headroom* requirement
    // applies, and therefore not the minimum size requirement.

    let program_address = Pubkey::new_unique();
    let upgrade_authority = Keypair::new();

    for headroom in [69, 420, 10_000, MINIMUM_EXTEND_PROGRAM_BYTES - 1] {
        let programdata_len = (MAX_PERMITTED_DATA_LENGTH as usize) - (headroom as usize);
        let mut context = setup_test_context_for_simd_0431_tests(
            &program_address,
            &upgrade_authority.pubkey(),
            programdata_len,
        )
        .await;

        // Anything below the headroom requirement should fail.
        let mut additional_bytes = 1;
        while additional_bytes < headroom - 1 {
            let payer_address = context.payer.pubkey();
            assert_ix_error(
                &mut context,
                extend_program(&program_address, Some(&payer_address), additional_bytes),
                None,
                InstructionError::InvalidArgument,
                "should fail because the requested extension is below the headroom",
            )
            .await;

            additional_bytes += headroom.div_ceil(5);
        }

        // The 10 KiB minimum should fail (too large).
        {
            let payer_address = context.payer.pubkey();
            assert_ix_error(
                &mut context,
                extend_program(
                    &program_address,
                    Some(&payer_address),
                    MINIMUM_EXTEND_PROGRAM_BYTES,
                ),
                None,
                InstructionError::InvalidRealloc,
                "should fail because the requested extension is too large",
            )
            .await;
        }

        // Only exactly `headroom` succeeds.
        {
            let client = &mut context.banks_client;
            let payer = &context.payer;
            let recent_blockhash = context.last_blockhash;
            let transaction = Transaction::new_signed_with_payer(
                &[extend_program(
                    &program_address,
                    Some(&payer.pubkey()),
                    headroom,
                )],
                Some(&payer.pubkey()),
                &[payer],
                recent_blockhash,
            );

            let (programdata_address, _) =
                Pubkey::find_program_address(&[program_address.as_ref()], &id());

            assert_matches!(client.process_transaction(transaction).await, Ok(()));
            let updated_program_data_account = client
                .get_account(programdata_address)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                updated_program_data_account.data().len(),
                programdata_len + (headroom as usize),
            );
        }
    }
}
