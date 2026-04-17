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
use solana_runtime::loader_utils::{load_upgradeable_program_and_advance_slot, load_upgradeable_program_wrapper};

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
                enable_return_data_recording: false,
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
    let ix_1 = Instruction::new_with_bytes(program_id_1, b"IX1", metas_for_ix_1);

    let metas_for_ix_2 = vec![
        AccountMeta::new_readonly(acc_2_keypair.pubkey(), true),
        AccountMeta::new(acc_3_key, false),
    ];
    let ix_2 = Instruction::new_with_bytes(program_id_2, b"IX2", metas_for_ix_2);

    let message = Message::new(&[ix_1, ix_2], Some(&mint_keypair.pubkey()));
    let bank_client = BankClient::new_shared(bank.clone());

    let result = bank_client
        .send_and_confirm_message(&[&mint_keypair, &acc_1_keypair, &acc_2_keypair], message);
    std::println!("result: {:?}", result);
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
