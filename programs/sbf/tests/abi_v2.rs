#![cfg(feature = "abi-v2")]

use {
    solana_account::{AccountSharedData, ReadableAccount},
    solana_client_traits::SyncClient,
    solana_instruction::{AccountMeta, Instruction},
    solana_keypair::Keypair,
    solana_message::{Message, inner_instruction::InnerInstruction},
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::Bank,
        bank_client::BankClient,
        genesis_utils::{GenesisConfigInfo, create_genesis_config},
        loader_utils::load_upgradeable_program_and_advance_slot,
    },
    solana_sdk_ids::system_program,
    solana_signer::Signer,
    solana_svm::{
        transaction_commit_result::{CommittedTransaction, TransactionCommitResult},
        transaction_processor::ExecutionRecordingConfig,
    },
    solana_svm_timings::ExecuteTimings,
    solana_transaction::Transaction,
    solana_transaction_error::TransactionError,
    std::sync::Arc,
};

fn process_transaction_and_record_inner(
    bank: &Bank,
    tx: Transaction,
) -> (
    Result<(), TransactionError>,
    Vec<Vec<InnerInstruction>>,
    Vec<String>,
    u64,
) {
    let commit_result = load_execute_and_commit_transaction(bank, tx);
    let CommittedTransaction {
        inner_instructions,
        log_messages,
        status,
        executed_units,
        ..
    } = commit_result.unwrap();
    let inner_instructions = inner_instructions.expect("cpi recording should be enabled");
    let log_messages = log_messages.expect("log recording should be enabled");
    (status, inner_instructions, log_messages, executed_units)
}

fn load_execute_and_commit_transaction(bank: &Bank, tx: Transaction) -> TransactionCommitResult {
    let txs = vec![tx];
    let tx_batch = bank.prepare_batch_for_tests(txs);
    let mut commit_results = bank
        .load_execute_and_commit_transactions(
            &tx_batch,
            ExecutionRecordingConfig {
                enable_cpi_recording: true,
                enable_log_recording: true,
                enable_return_data_recording: true,
                enable_transaction_balance_recording: false,
            },
            &mut ExecuteTimings::default(),
            None,
        )
        .0;
    commit_results.pop().unwrap()
}

#[test]
fn regions_sanity_test() {
    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50);

    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let mut bank_client = BankClient::new_shared(bank.clone());
    let authority_keypair = Keypair::new();

    let (_bank, program_id_1) = load_upgradeable_program_and_advance_slot(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_abi_v2_memory",
    );

    let (bank, program_id_2) = load_upgradeable_program_and_advance_slot(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_abi_v2_memory",
    );

    let acc_1_keypair = Keypair::new();
    let acc_1 = AccountSharedData::create_from_existing_shared_data(
        223450,
        vec![1, 2, 3].into(),
        system_program::id(),
        false,
        64,
    );
    bank.store_account(&acc_1_keypair.pubkey(), &acc_1);

    let acc_2_keypair = Keypair::new();
    let acc_2 = AccountSharedData::create_from_existing_shared_data(
        35,
        vec![3, 4, 5].into(),
        system_program::id(),
        false,
        64,
    );
    bank.store_account(&acc_2_keypair.pubkey(), &acc_2);

    let acc_3_key = Pubkey::new_unique();
    let acc_3 = AccountSharedData::create_from_existing_shared_data(
        9123,
        vec![6, 7, 8].into(),
        acc_2_keypair.pubkey(),
        false,
        64,
    );
    bank.store_account(&acc_3_key, &acc_3);

    let acc_4_key = Pubkey::new_unique();
    let acc_4 = AccountSharedData::create_from_existing_shared_data(
        90123,
        Arc::new(acc_4_key.to_bytes().to_vec()),
        acc_2_keypair.pubkey(),
        false,
        64,
    );
    bank.store_account(&acc_4_key, &acc_4);

    let metas_for_ix_1 = vec![
        AccountMeta::new(acc_1_keypair.pubkey(), true),
        AccountMeta::new_readonly(acc_4_key, false),
    ];
    let ix_1 = Instruction::new_with_bytes(program_id_1, b"\x02", metas_for_ix_1);

    let metas_for_ix_2 = vec![
        AccountMeta::new_readonly(acc_2_keypair.pubkey(), true),
        AccountMeta::new(acc_3_key, false),
    ];
    let ix_2 = Instruction::new_with_bytes(program_id_2, b"\x03", metas_for_ix_2);

    let message = Message::new(&[ix_1, ix_2], Some(&mint_keypair.pubkey()));
    let bank_client = BankClient::new_shared(bank.clone());

    let result = bank_client
        .send_and_confirm_message(&[&mint_keypair, &acc_1_keypair, &acc_2_keypair], message);
    std::eprintln!("result: {:?}", result);
    assert!(result.is_ok());
}

#[test]
fn test_access_invalid_regions() {
    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50);

    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let mut bank_client = BankClient::new_shared(bank.clone());
    let authority_keypair = Keypair::new();

    let (bank, program_id_1) = load_upgradeable_program_and_advance_slot(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_abi_v2_memory",
    );

    let acc_1_key = Pubkey::new_unique();
    let acc_1 = AccountSharedData::create_from_existing_shared_data(
        223450,
        vec![1, 2, 3].into(),
        system_program::id(),
        false,
        64,
    );
    bank.store_account(&acc_1_key, &acc_1);

    let acc_2_key = Pubkey::new_unique();
    let acc_2 = AccountSharedData::create_from_existing_shared_data(
        90123,
        vec![4, 5, 6].into(),
        acc_1_key,
        false,
        64,
    );
    bank.store_account(&acc_2_key, &acc_2);

    let metas_for_ix_1 = vec![
        AccountMeta::new(acc_1_key, false),
        AccountMeta::new_readonly(acc_2_key, false),
    ];

    let check_invalid_access = |area: u64| {
        let mut data = area.to_le_bytes().to_vec();
        data.insert(0, 0);
        let ix_1 = Instruction::new_with_bytes(program_id_1, &data, metas_for_ix_1.clone());
        let message = Message::new(&[ix_1], Some(&mint_keypair.pubkey()));
        let tx = Transaction::new(&[&mint_keypair], message, bank.last_blockhash());

        let (_, _, logs, _) = process_transaction_and_record_inner(&bank, tx);
        let last_line = logs.last().unwrap();
        std::println!("logs: {:?}", logs);
        // assert_eq!(last_line, &format!("Access violation in unknown section at address 0x{:x}00000000", i));
        assert!(last_line.contains("Access violation"));
        assert!(
            last_line.contains(&format!("at address 0x{:x}0000000", area)),
            "{last_line}"
        );
    };

    for i in 0x1a..0x108u64 {
        check_invalid_access(i);
    }

    for i in 0x109..0x148u64 {
        check_invalid_access(i);
    }

    for i in 0x149..0x160 {
        check_invalid_access(i);
    }
}

#[test]
fn test_write_to_accounts() {
    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50);

    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let mut bank_client = BankClient::new_shared(bank.clone());
    let authority_keypair = Keypair::new();

    let (bank, program_id) = load_upgradeable_program_and_advance_slot(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_abi_v2_memory",
    );

    let acc_1_key = Pubkey::new_unique();
    let acc_1 = AccountSharedData::create_from_existing_shared_data(
        223450,
        vec![1, 2, 3].into(),
        program_id,
        false,
        64,
    );
    bank.store_account(&acc_1_key, &acc_1);

    let acc_2_key = Pubkey::new_unique();
    let acc_2 = AccountSharedData::create_from_existing_shared_data(
        90123,
        vec![4, 5, 6].into(),
        program_id,
        false,
        64,
    );
    bank.store_account(&acc_2_key, &acc_2);

    let metas_for_ix_1 = vec![
        AccountMeta::new(acc_1_key, false),
        AccountMeta::new_readonly(acc_2_key, false),
    ];

    let data = [1u8];
    let ix_1 = Instruction::new_with_bytes(program_id, &data, metas_for_ix_1.clone());
    let message = Message::new(&[ix_1], Some(&mint_keypair.pubkey()));
    let bank_client = BankClient::new_shared(bank.clone());

    let result = bank_client.send_and_confirm_message(&[&mint_keypair], message);
    assert!(result.is_ok());

    let account = bank.get_account_with_fixed_root(&acc_1_key).unwrap();
    assert_eq!(account.data(), &[7, 8, 9]);
}

#[test]
fn account_permissions_update() {
    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50);

    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let mut bank_client = BankClient::new_shared(bank.clone());
    let authority_keypair = Keypair::new();

    let (bank, program_id) = load_upgradeable_program_and_advance_slot(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_abi_v2_memory",
    );

    let acc_1_key = Pubkey::new_unique();
    let acc_1 = AccountSharedData::create_from_existing_shared_data(
        223450,
        vec![1, 2, 3].into(),
        program_id,
        false,
        64,
    );
    bank.store_account(&acc_1_key, &acc_1);

    let acc_2_key = Pubkey::new_unique();
    let acc_2 = AccountSharedData::create_from_existing_shared_data(
        90123,
        vec![4, 5, 6].into(),
        program_id,
        false,
        64,
    );
    bank.store_account(&acc_2_key, &acc_2);

    let acc_3_key = Pubkey::new_unique();
    let acc_3 = AccountSharedData::create_from_existing_shared_data(
        90123,
        vec![4, 5, 6].into(),
        system_program::id(),
        false,
        64,
    );
    bank.store_account(&acc_3_key, &acc_3);

    let metas_for_ix_1 = vec![
        AccountMeta::new(acc_1_key, false),
        AccountMeta::new_readonly(acc_2_key, false),
    ];

    let data = [1u8];
    let ix_1 = Instruction::new_with_bytes(program_id, &data, metas_for_ix_1);

    let metas_for_ix_2 = vec![
        AccountMeta::new_readonly(acc_2_key, false),
        AccountMeta::new_readonly(acc_1_key, false),
    ];
    let data = [1u8];
    let ix_2 = Instruction::new_with_bytes(program_id, &data, metas_for_ix_2);

    let metas_for_ix_3 = vec![
        AccountMeta::new(acc_3_key, false),
        AccountMeta::new_readonly(acc_1_key, false),
    ];
    let data = [1u8];
    let ix_3 = Instruction::new_with_bytes(program_id, &data, metas_for_ix_3);

    let message = Message::new(&[ix_1, ix_2], Some(&mint_keypair.pubkey()));
    let tx = Transaction::new(&[&mint_keypair], message, bank.last_blockhash());
    let (_, _, logs, _) = process_transaction_and_record_inner(&bank, tx);
    let first_ix = logs.get(2).unwrap();
    assert!(first_ix.contains("success"));

    let second_ix = logs.last().unwrap();
    assert!(second_ix.contains("Access violation"));

    let message = Message::new(&[ix_3], Some(&mint_keypair.pubkey()));
    let tx = Transaction::new(&[&mint_keypair], message, bank.last_blockhash());
    let (_, _, logs, _) = process_transaction_and_record_inner(&bank, tx);
    let third_ix = logs.get(2).unwrap();
    assert!(third_ix.contains("Access violation"));
}

fn common_set_buffer_length(test_discr: u8) -> CommittedTransaction {
    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50);
    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let mut bank_client = BankClient::new_shared(bank.clone());
    let authority_keypair = Keypair::new();
    let (bank, program_id) = load_upgradeable_program_and_advance_slot(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_abi_v2_memory",
    );
    let acc_1_key = Pubkey::new_unique();
    let acc_1 = AccountSharedData::create_from_existing_shared_data(
        9999,
        vec![1, 2, 3].into(),
        program_id,
        false,
        64,
    );
    bank.store_account(&acc_1_key, &acc_1);

    let acc_2_key = Pubkey::new_unique();
    let acc_2 = AccountSharedData::create_from_existing_shared_data(
        10001,
        vec![7, 8, 9].into(),
        system_program::id(),
        false,
        64,
    );
    bank.store_account(&acc_2_key, &acc_2);

    let acc_3_key = Pubkey::new_unique();
    let acc_3 = AccountSharedData::create_from_existing_shared_data(
        10000,
        vec![4, 5, 6].into(),
        program_id,
        false,
        64,
    );
    bank.store_account(&acc_3_key, &acc_3);

    let metas_for_ix_1 = vec![
        AccountMeta::new(acc_1_key, false),
        AccountMeta::new(acc_2_key, false),
        AccountMeta::new_readonly(acc_3_key, false),
    ];

    let data = [test_discr];
    let ix_1 = Instruction::new_with_bytes(program_id, &data, metas_for_ix_1.clone());
    let message = Message::new(&[ix_1], Some(&mint_keypair.pubkey()));
    let tx = Transaction::new(&[&mint_keypair], message, bank.last_blockhash());
    let commit_result = load_execute_and_commit_transaction(&bank, tx).unwrap();
    std::eprintln!("logs: {:?}", commit_result.log_messages);
    commit_result
}

#[test]
fn buffer_resize_return_scratchpad_success() {
    // Verify that we resize to exactly the requested length and can write to the new data.
    let result = common_set_buffer_length(0x04);
    result.status.expect("success");
    let return_data = result.return_data.expect("should have return data");
    assert_eq!(256, return_data.data.len());
    assert_eq!(return_data.data[127], 42);
}

#[test]
fn buffer_resize_return_scratchpad_oob_access() {
    // Verify that we resize to exactly the requested length, by reading one byte past the
    // requested buffer size.
    let result = common_set_buffer_length(0x05);
    result.status.expect_err("err");
    assert_eq!(128, result.return_data.unwrap().data.len());
    assert!(
        result
            .log_messages
            .unwrap()
            .last()
            .unwrap()
            .contains("Access violation")
    );
}

#[test]
fn buffer_resize_writable_account() {
    let result = common_set_buffer_length(0x06);
    result.status.expect("success");
}

#[test]
fn buffer_resize_readonly_account() {
    let result = common_set_buffer_length(0x07);
    result.status.expect_err("err");
    assert!(
        result
            .log_messages
            .unwrap()
            .last()
            .unwrap()
            .contains("instruction modified data of a read-only account")
    );
}

#[test]
fn buffer_resize_somebody_elses_account() {
    let result = common_set_buffer_length(0x08);
    result.status.expect_err("err");
    assert!(
        result
            .log_messages
            .unwrap()
            .last()
            .unwrap()
            .contains("instruction modified data of an account it does not own")
    );
}

#[test]
fn test_assign_owner() {
    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50);

    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let mut bank_client = BankClient::new_shared(bank.clone());
    let authority_keypair = Keypair::new();

    let (bank, program_id) = load_upgradeable_program_and_advance_slot(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_abi_v2_memory",
    );

    let acc_1_key = Pubkey::new_unique();
    let new_owner = Pubkey::new_unique();

    let mut payload = new_owner.as_array().to_vec();
    payload.push(0);
    let mut acc_1 = AccountSharedData::create_from_existing_shared_data(
        223450,
        Arc::new(payload.clone()),
        program_id,
        false,
        64,
    );
    bank.store_account(&acc_1_key, &acc_1);

    let acc_2_key = Pubkey::new_unique();
    let acc_2 = AccountSharedData::create_from_existing_shared_data(
        90123,
        Arc::new(Vec::new()),
        program_id,
        false,
        64,
    );
    bank.store_account(&acc_2_key, &acc_2);

    let acc_3_key = Pubkey::new_unique();
    let acc_3 = AccountSharedData::create_from_existing_shared_data(
        897,
        Arc::new(Vec::new()),
        program_id,
        false,
        90,
    );
    bank.store_account(&acc_3_key, &acc_3);

    let metas_for_ix_1 = vec![
        AccountMeta::new_readonly(acc_1_key, false),
        AccountMeta::new(acc_2_key, false),
    ];
    let ix_1 = Instruction::new_with_bytes(program_id, &[9], metas_for_ix_1);
    let message = Message::new(&[ix_1], Some(&mint_keypair.pubkey()));
    let tx = Transaction::new(&[&mint_keypair], message, bank.last_blockhash());
    let (_, _, logs, _) = process_transaction_and_record_inner(&bank, tx);
    assert!(logs.last().unwrap().contains("success"));

    let acc_2 = bank.get_account(&acc_2_key).unwrap();
    assert_eq!(acc_2.owner(), &new_owner);

    // Try writing to the account afterwards
    *payload.last_mut().unwrap() = 1;
    acc_1.set_data(payload);
    bank.store_account(&acc_1_key, &acc_1);

    let metas_for_ix_2 = vec![
        AccountMeta::new_readonly(acc_1_key, false),
        AccountMeta::new(acc_3_key, false),
    ];
    let ix_1 = Instruction::new_with_bytes(program_id, &[9], metas_for_ix_2);
    let message = Message::new(&[ix_1], Some(&mint_keypair.pubkey()));
    let tx = Transaction::new(&[&mint_keypair], message, bank.last_blockhash());
    let (_, _, logs, _) = process_transaction_and_record_inner(&bank, tx);
    assert!(
        logs.last()
            .unwrap()
            .contains("Access violation in unknown section at address 0x900000000 of size 1")
    );
}

#[test]
fn test_sol_transfer_lamports() {
    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50);

    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let mut bank_client = BankClient::new_shared(bank.clone());
    let authority_keypair = Keypair::new();

    let (bank, program_id) = load_upgradeable_program_and_advance_slot(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_abi_v2_memory",
    );

    let acc_1_key = Pubkey::new_unique();
    let acc_1 = AccountSharedData::create_from_existing_shared_data(
        10,
        vec![1, 2, 3].into(),
        program_id,
        false,
        64,
    );
    bank.store_account(&acc_1_key, &acc_1);

    let acc_2_key = Pubkey::new_unique();
    let acc_2 = AccountSharedData::create_from_existing_shared_data(
        40,
        vec![4, 5, 6].into(),
        program_id,
        false,
        64,
    );
    bank.store_account(&acc_2_key, &acc_2);

    let metas_for_ix_1 = vec![
        AccountMeta::new(acc_1_key, false),
        AccountMeta::new(acc_2_key, false),
    ];

    let data = [10u8];
    let ix_1 = Instruction::new_with_bytes(program_id, &data, metas_for_ix_1);
    let message = Message::new(&[ix_1], Some(&mint_keypair.pubkey()));
    let tx = Transaction::new(&[&mint_keypair], message, bank.last_blockhash());
    let (_, _, logs, _) = process_transaction_and_record_inner(&bank, tx);

    assert!(logs.last().unwrap().contains("success"));
    assert_eq!(bank.get_account(&acc_1_key).unwrap().lamports(), 20);
    assert_eq!(bank.get_account(&acc_2_key).unwrap().lamports(), 30);
}
