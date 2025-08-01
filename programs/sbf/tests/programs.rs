#![cfg(any(feature = "sbf_c", feature = "sbf_rust"))]
#![allow(clippy::clone_on_copy)]
#![allow(clippy::needless_range_loop)]
#![allow(clippy::needless_borrow)]
#![allow(clippy::cmp_owned)]
#![allow(clippy::match_like_matches_macro)]
#![allow(clippy::unnecessary_cast)]
#![allow(clippy::uninlined_format_args)]

#[cfg(feature = "sbf_rust")]
use {
    agave_feature_set::{self as feature_set, FeatureSet},
    agave_reserved_account_keys::ReservedAccountKeys,
    borsh::{from_slice, to_vec, BorshDeserialize, BorshSerialize},
    solana_account::{AccountSharedData, ReadableAccount, WritableAccount},
    solana_account_info::MAX_PERMITTED_DATA_INCREASE,
    solana_client_traits::SyncClient,
    solana_clock::{UnixTimestamp, MAX_PROCESSING_AGE},
    solana_compute_budget::compute_budget::ComputeBudget,
    solana_compute_budget_instruction::instructions_processor::process_compute_budget_instructions,
    solana_compute_budget_interface::ComputeBudgetInstruction,
    solana_fee_calculator::FeeRateGovernor,
    solana_fee_structure::{FeeBin, FeeBudgetLimits, FeeStructure},
    solana_genesis_config::ClusterType,
    solana_hash::Hash,
    solana_instruction::{error::InstructionError, AccountMeta, Instruction},
    solana_keypair::Keypair,
    solana_loader_v3_interface::instruction as loader_v3_instruction,
    solana_loader_v4_interface::instruction as loader_v4_instruction,
    solana_message::{Message, SanitizedMessage},
    solana_program_runtime::invoke_context::mock_process_instruction,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_runtime::{
        bank::Bank,
        bank_client::BankClient,
        bank_forks::BankForks,
        genesis_utils::{
            bootstrap_validator_stake_lamports, create_genesis_config,
            create_genesis_config_with_leader_ex, GenesisConfigInfo,
        },
        loader_utils::{
            create_program, instructions_to_load_program_of_loader_v4, load_program_from_file,
            load_program_of_loader_v4, load_upgradeable_buffer,
        },
    },
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_sbf_rust_invoke_dep::*,
    solana_sbf_rust_realloc_dep::*,
    solana_sbf_rust_realloc_invoke_dep::*,
    solana_sbpf::vm::ContextObject,
    solana_sdk_ids::sysvar::{self as sysvar, clock},
    solana_sdk_ids::{bpf_loader, bpf_loader_deprecated, bpf_loader_upgradeable, loader_v4},
    solana_signer::Signer,
    solana_stake_interface as stake,
    solana_svm::{
        transaction_commit_result::{CommittedTransaction, TransactionCommitResult},
        transaction_execution_result::InnerInstruction,
        transaction_processor::ExecutionRecordingConfig,
    },
    solana_svm_transaction::svm_message::SVMMessage,
    solana_system_interface::{program as system_program, MAX_PERMITTED_DATA_LENGTH},
    solana_timings::ExecuteTimings,
    solana_transaction::Transaction,
    solana_transaction_error::TransactionError,
    solana_type_overrides::rand,
    std::{
        assert_eq,
        cell::RefCell,
        str::FromStr,
        sync::{Arc, RwLock},
        time::Duration,
    },
};

#[cfg(feature = "sbf_rust")]
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

#[cfg(feature = "sbf_rust")]
fn load_execute_and_commit_transaction(bank: &Bank, tx: Transaction) -> TransactionCommitResult {
    let txs = vec![tx];
    let tx_batch = bank.prepare_batch_for_tests(txs);
    let mut commit_results = bank
        .load_execute_and_commit_transactions(
            &tx_batch,
            MAX_PROCESSING_AGE,
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

#[cfg(feature = "sbf_rust")]
fn bank_with_feature_activated(
    bank_forks: &RwLock<BankForks>,
    parent: Arc<Bank>,
    feature_id: &Pubkey,
) -> Arc<Bank> {
    let slot = parent.slot().saturating_add(1);
    let mut bank = Bank::new_from_parent(parent, &Pubkey::new_unique(), slot);
    bank.activate_feature(feature_id);
    bank_forks
        .write()
        .unwrap()
        .insert(bank)
        .clone_without_scheduler()
}

#[cfg(feature = "sbf_rust")]
fn bank_with_feature_deactivated(
    bank_forks: &RwLock<BankForks>,
    parent: Arc<Bank>,
    feature_id: &Pubkey,
) -> Arc<Bank> {
    let slot = parent.slot().saturating_add(1);
    let mut bank = Bank::new_from_parent(parent, &Pubkey::new_unique(), slot);
    bank.deactivate_feature(feature_id);
    bank_forks
        .write()
        .unwrap()
        .insert(bank)
        .clone_without_scheduler()
}

#[cfg(feature = "sbf_rust")]
const LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST: u32 = 64 * 1024 * 1024;

#[test]
#[cfg(any(feature = "sbf_c", feature = "sbf_rust"))]
fn test_program_sbf_sanity() {
    solana_logger::setup();

    let mut programs = Vec::new();
    #[cfg(feature = "sbf_c")]
    {
        programs.extend_from_slice(&[
            ("alloc", true),
            ("alt_bn128", true),
            ("alt_bn128_compression", true),
            ("sbf_to_sbf", true),
            ("float", true),
            ("multiple_static", true),
            ("noop", true),
            ("noop++", true),
            ("panic", false),
            ("poseidon", true),
            ("relative_call", true),
            ("return_data", true),
            ("sanity", true),
            ("sanity++", true),
            ("secp256k1_recover", true),
            ("sha", true),
            ("stdlib", true),
            ("struct_pass", true),
            ("struct_ret", true),
        ]);
    }
    #[cfg(feature = "sbf_rust")]
    {
        programs.extend_from_slice(&[
            ("solana_sbf_rust_128bit", true),
            ("solana_sbf_rust_alloc", true),
            ("solana_sbf_rust_alt_bn128", true),
            ("solana_sbf_rust_alt_bn128_compression", true),
            ("solana_sbf_rust_curve25519", true),
            ("solana_sbf_rust_custom_heap", true),
            ("solana_sbf_rust_dep_crate", true),
            ("solana_sbf_rust_external_spend", false),
            ("solana_sbf_rust_iter", true),
            ("solana_sbf_rust_many_args", true),
            ("solana_sbf_rust_mem", true),
            ("solana_sbf_rust_membuiltins", true),
            ("solana_sbf_rust_noop", true),
            ("solana_sbf_rust_panic", false),
            ("solana_sbf_rust_param_passing", true),
            ("solana_sbf_rust_poseidon", true),
            ("solana_sbf_rust_rand", true),
            ("solana_sbf_rust_remaining_compute_units", true),
            ("solana_sbf_rust_sanity", true),
            ("solana_sbf_rust_secp256k1_recover", true),
            ("solana_sbf_rust_sha", true),
        ]);
    }

    #[cfg(all(feature = "sbf_rust", feature = "sbf_sanity_list"))]
    {
        // This code generates the list of sanity programs for a CI job to build with
        // cargo-build-sbf and ensure it is working correctly.
        use std::{env, fs::File, io::Write};
        let current_dir = env::current_dir().unwrap();
        let mut file = File::create(current_dir.join("sanity_programs.txt")).unwrap();
        for program in programs.iter() {
            writeln!(file, "{}", program.0.trim_start_matches("solana_sbf_rust_"))
                .expect("Failed to write to file");
        }
    }

    #[cfg(not(feature = "sbf_sanity_list"))]
    for program in programs.iter() {
        println!("Test program: {:?}", program.0);

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(50);

        let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        let mut bank_client = BankClient::new_shared(bank);
        let authority_keypair = Keypair::new();

        // Call user program
        let (_bank, program_id) = load_program_of_loader_v4(
            &mut bank_client,
            &bank_forks,
            &mint_keypair,
            &authority_keypair,
            program.0,
        );

        let account_metas = vec![
            AccountMeta::new(mint_keypair.pubkey(), true),
            AccountMeta::new(Keypair::new().pubkey(), false),
        ];
        let instruction = Instruction::new_with_bytes(program_id, &[1], account_metas);
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        if program.1 {
            assert!(result.is_ok(), "{result:?}");
        } else {
            assert!(result.is_err(), "{result:?}");
        }
    }
}

#[test]
#[cfg(any(feature = "sbf_c", feature = "sbf_rust"))]
fn test_program_sbf_loader_deprecated() {
    solana_logger::setup();

    let mut programs = Vec::new();
    #[cfg(feature = "sbf_c")]
    {
        programs.extend_from_slice(&[("deprecated_loader")]);
    }
    #[cfg(feature = "sbf_rust")]
    {
        programs.extend_from_slice(&[("solana_sbf_rust_deprecated_loader")]);
    }

    for program in programs.iter() {
        println!("Test program: {:?}", program);

        let GenesisConfigInfo {
            mut genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(50);
        genesis_config
            .accounts
            .remove(&agave_feature_set::disable_deploy_of_alloc_free_syscall::id())
            .unwrap();
        let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        let program_id = create_program(&bank, &bpf_loader_deprecated::id(), program);

        let mut bank_client = BankClient::new_shared(bank);
        bank_client
            .advance_slot(1, bank_forks.as_ref(), &Pubkey::default())
            .expect("Failed to advance the slot");
        let account_metas = vec![AccountMeta::new(mint_keypair.pubkey(), true)];
        let instruction = Instruction::new_with_bytes(program_id, &[255], account_metas);
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        assert!(result.is_ok());
    }
}

#[test]
#[cfg(feature = "sbf_rust")]
#[should_panic(
    expected = "called `Result::unwrap()` on an `Err` value: TransactionError(InstructionError(0, InvalidAccountData))"
)]
fn test_sol_alloc_free_no_longer_deployable_with_upgradeable_loader() {
    solana_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50);

    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let mut bank_client = BankClient::new_shared(bank.clone());
    let authority_keypair = Keypair::new();

    // Populate loader account with `solana_sbf_rust_deprecated_loader` elf, which
    // depends on `sol_alloc_free_` syscall. This can be verified with
    // $ elfdump solana_sbf_rust_deprecated_loader.so
    // : 0000000000001ab8  000000070000000a R_BPF_64_32            0000000000000000 sol_alloc_free_
    // In the symbol table, there is `sol_alloc_free_`.
    // In fact, `sol_alloc_free_` is called from sbf allocator, which is originated from
    // AccountInfo::realloc() in the program code.

    // Expect that deployment to fail. B/C during deployment, there is an elf
    // verification step, which uses the runtime to look up relocatable symbols
    // in elf inside syscall table. In this case, `sol_alloc_free_` can't be
    // found in syscall table. Hence, the verification fails and the deployment
    // fails.
    let (_bank, _program_id) = load_program_of_loader_v4(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_deprecated_loader",
    );
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_duplicate_accounts() {
    solana_logger::setup();

    let mut programs = Vec::new();
    #[cfg(feature = "sbf_c")]
    {
        programs.extend_from_slice(&[("dup_accounts")]);
    }
    #[cfg(feature = "sbf_rust")]
    {
        programs.extend_from_slice(&[("solana_sbf_rust_dup_accounts")]);
    }

    for program in programs.iter() {
        println!("Test program: {:?}", program);

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(50);

        let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        let mut bank_client = BankClient::new_shared(bank.clone());
        let authority_keypair = Keypair::new();

        let (bank, program_id) = load_program_of_loader_v4(
            &mut bank_client,
            &bank_forks,
            &mint_keypair,
            &authority_keypair,
            program,
        );
        let payee_account = AccountSharedData::new(10, 1, &program_id);
        let payee_pubkey = Pubkey::new_unique();
        bank.store_account(&payee_pubkey, &payee_account);
        let account = AccountSharedData::new(10, 1, &program_id);

        let pubkey = Pubkey::new_unique();
        let account_metas = vec![
            AccountMeta::new(mint_keypair.pubkey(), true),
            AccountMeta::new(payee_pubkey, false),
            AccountMeta::new(pubkey, false),
            AccountMeta::new(pubkey, false),
        ];

        bank.store_account(&pubkey, &account);
        let instruction = Instruction::new_with_bytes(program_id, &[1], account_metas.clone());
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        let data = bank_client.get_account_data(&pubkey).unwrap().unwrap();
        assert!(result.is_ok());
        assert_eq!(data[0], 1);

        bank.store_account(&pubkey, &account);
        let instruction = Instruction::new_with_bytes(program_id, &[2], account_metas.clone());
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        let data = bank_client.get_account_data(&pubkey).unwrap().unwrap();
        assert!(result.is_ok());
        assert_eq!(data[0], 2);

        bank.store_account(&pubkey, &account);
        let instruction = Instruction::new_with_bytes(program_id, &[3], account_metas.clone());
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        let data = bank_client.get_account_data(&pubkey).unwrap().unwrap();
        assert!(result.is_ok());
        assert_eq!(data[0], 3);

        bank.store_account(&pubkey, &account);
        let instruction = Instruction::new_with_bytes(program_id, &[4], account_metas.clone());
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        let lamports = bank_client.get_balance(&pubkey).unwrap();
        assert!(result.is_ok());
        assert_eq!(lamports, 11);

        bank.store_account(&pubkey, &account);
        let instruction = Instruction::new_with_bytes(program_id, &[5], account_metas.clone());
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        let lamports = bank_client.get_balance(&pubkey).unwrap();
        assert!(result.is_ok());
        assert_eq!(lamports, 12);

        bank.store_account(&pubkey, &account);
        let instruction = Instruction::new_with_bytes(program_id, &[6], account_metas.clone());
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        let lamports = bank_client.get_balance(&pubkey).unwrap();
        assert!(result.is_ok());
        assert_eq!(lamports, 13);

        let keypair = Keypair::new();
        let pubkey = keypair.pubkey();
        let account_metas = vec![
            AccountMeta::new(mint_keypair.pubkey(), true),
            AccountMeta::new(payee_pubkey, false),
            AccountMeta::new(pubkey, false),
            AccountMeta::new_readonly(pubkey, true),
            AccountMeta::new_readonly(program_id, false),
        ];
        bank.store_account(&pubkey, &account);
        let instruction = Instruction::new_with_bytes(program_id, &[7], account_metas.clone());
        let message = Message::new(&[instruction], Some(&mint_keypair.pubkey()));
        let result = bank_client.send_and_confirm_message(&[&mint_keypair, &keypair], message);
        assert!(result.is_ok());
    }
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_error_handling() {
    solana_logger::setup();

    let mut programs = Vec::new();
    #[cfg(feature = "sbf_c")]
    {
        programs.extend_from_slice(&[("error_handling")]);
    }
    #[cfg(feature = "sbf_rust")]
    {
        programs.extend_from_slice(&[("solana_sbf_rust_error_handling")]);
    }

    for program in programs.iter() {
        println!("Test program: {:?}", program);

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(50);

        let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        let mut bank_client = BankClient::new_shared(bank);
        let authority_keypair = Keypair::new();

        let (_bank, program_id) = load_program_of_loader_v4(
            &mut bank_client,
            &bank_forks,
            &mint_keypair,
            &authority_keypair,
            program,
        );

        let account_metas = vec![AccountMeta::new(mint_keypair.pubkey(), true)];

        let instruction = Instruction::new_with_bytes(program_id, &[1], account_metas.clone());
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        assert!(result.is_ok());

        let instruction = Instruction::new_with_bytes(program_id, &[2], account_metas.clone());
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        assert_eq!(
            result.unwrap_err().unwrap(),
            TransactionError::InstructionError(0, InstructionError::InvalidAccountData)
        );

        let instruction = Instruction::new_with_bytes(program_id, &[3], account_metas.clone());
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        assert_eq!(
            result.unwrap_err().unwrap(),
            TransactionError::InstructionError(0, InstructionError::Custom(0))
        );

        let instruction = Instruction::new_with_bytes(program_id, &[4], account_metas.clone());
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        assert_eq!(
            result.unwrap_err().unwrap(),
            TransactionError::InstructionError(0, InstructionError::Custom(42))
        );

        let instruction = Instruction::new_with_bytes(program_id, &[5], account_metas.clone());
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        let result = result.unwrap_err().unwrap();
        if TransactionError::InstructionError(0, InstructionError::InvalidInstructionData) != result
        {
            assert_eq!(
                result,
                TransactionError::InstructionError(0, InstructionError::InvalidError)
            );
        }

        let instruction = Instruction::new_with_bytes(program_id, &[6], account_metas.clone());
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        let result = result.unwrap_err().unwrap();
        if TransactionError::InstructionError(0, InstructionError::InvalidInstructionData) != result
        {
            assert_eq!(
                result,
                TransactionError::InstructionError(0, InstructionError::InvalidError)
            );
        }

        let instruction = Instruction::new_with_bytes(program_id, &[7], account_metas.clone());
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        let result = result.unwrap_err().unwrap();
        if TransactionError::InstructionError(0, InstructionError::InvalidInstructionData) != result
        {
            assert_eq!(
                result,
                TransactionError::InstructionError(0, InstructionError::AccountBorrowFailed)
            );
        }

        let instruction = Instruction::new_with_bytes(program_id, &[8], account_metas.clone());
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        assert_eq!(
            result.unwrap_err().unwrap(),
            TransactionError::InstructionError(0, InstructionError::InvalidInstructionData)
        );

        let instruction = Instruction::new_with_bytes(program_id, &[9], account_metas.clone());
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        assert_eq!(
            result.unwrap_err().unwrap(),
            TransactionError::InstructionError(0, InstructionError::MaxSeedLengthExceeded)
        );
    }
}

#[test]
#[cfg(any(feature = "sbf_c", feature = "sbf_rust"))]
fn test_return_data_and_log_data_syscall() {
    solana_logger::setup();

    let mut programs = Vec::new();
    #[cfg(feature = "sbf_c")]
    {
        programs.extend_from_slice(&[("log_data")]);
    }
    #[cfg(feature = "sbf_rust")]
    {
        programs.extend_from_slice(&[("solana_sbf_rust_log_data")]);
    }

    for program in programs.iter() {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(50);

        let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        let mut bank_client = BankClient::new_shared(bank.clone());
        let authority_keypair = Keypair::new();

        let (bank, program_id) = load_program_of_loader_v4(
            &mut bank_client,
            &bank_forks,
            &mint_keypair,
            &authority_keypair,
            program,
        );

        bank.freeze();

        let account_metas = vec![AccountMeta::new(mint_keypair.pubkey(), true)];
        let instruction =
            Instruction::new_with_bytes(program_id, &[1, 2, 3, 0, 4, 5, 6], account_metas);

        let blockhash = bank.last_blockhash();
        let message = Message::new(&[instruction], Some(&mint_keypair.pubkey()));
        let transaction = Transaction::new(&[&mint_keypair], message, blockhash);
        let sanitized_tx = RuntimeTransaction::from_transaction_for_tests(transaction);

        let result = bank.simulate_transaction(&sanitized_tx, false);

        assert!(result.result.is_ok());

        assert_eq!(result.logs[1], "Program data: AQID BAUG");

        assert_eq!(
            result.logs[3],
            format!("Program return: {} CAFE", program_id)
        );
    }
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_invoke_sanity() {
    solana_logger::setup();

    #[derive(Debug)]
    #[allow(dead_code)]
    enum Languages {
        C,
        Rust,
    }
    let mut programs = Vec::new();
    #[cfg(feature = "sbf_c")]
    {
        programs.push((Languages::C, "invoke", "invoked", "noop"));
    }
    #[cfg(feature = "sbf_rust")]
    {
        programs.push((
            Languages::Rust,
            "solana_sbf_rust_invoke",
            "solana_sbf_rust_invoked",
            "solana_sbf_rust_noop",
        ));
    }
    for program in programs.iter() {
        println!("Test program: {:?}", program);

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(50);

        let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        let mut bank_client = BankClient::new_shared(bank.clone());
        let authority_keypair = Keypair::new();

        let (_bank, invoke_program_id) = load_program_of_loader_v4(
            &mut bank_client,
            &bank_forks,
            &mint_keypair,
            &authority_keypair,
            program.1,
        );
        let (_bank, invoked_program_id) = load_program_of_loader_v4(
            &mut bank_client,
            &bank_forks,
            &mint_keypair,
            &authority_keypair,
            program.2,
        );
        let (bank, noop_program_id) = load_program_of_loader_v4(
            &mut bank_client,
            &bank_forks,
            &mint_keypair,
            &authority_keypair,
            program.3,
        );

        let argument_keypair = Keypair::new();
        let account = AccountSharedData::new(42, 100, &invoke_program_id);
        bank.store_account(&argument_keypair.pubkey(), &account);

        let invoked_argument_keypair = Keypair::new();
        let account = AccountSharedData::new(20, 10, &invoked_program_id);
        bank.store_account(&invoked_argument_keypair.pubkey(), &account);

        let from_keypair = Keypair::new();
        let account = AccountSharedData::new(84, 0, &system_program::id());
        bank.store_account(&from_keypair.pubkey(), &account);

        let unexecutable_program_keypair = Keypair::new();
        let account = AccountSharedData::new(1, 0, &bpf_loader::id());
        bank.store_account(&unexecutable_program_keypair.pubkey(), &account);

        let (derived_key1, bump_seed1) =
            Pubkey::find_program_address(&[b"You pass butter"], &invoke_program_id);
        let (derived_key2, bump_seed2) =
            Pubkey::find_program_address(&[b"Lil'", b"Bits"], &invoked_program_id);
        let (derived_key3, bump_seed3) =
            Pubkey::find_program_address(&[derived_key2.as_ref()], &invoked_program_id);

        let mint_pubkey = mint_keypair.pubkey();
        let account_metas = vec![
            AccountMeta::new(mint_pubkey, true),
            AccountMeta::new(argument_keypair.pubkey(), true),
            AccountMeta::new_readonly(invoked_program_id, false),
            AccountMeta::new(invoked_argument_keypair.pubkey(), true),
            AccountMeta::new_readonly(invoked_program_id, false),
            AccountMeta::new(argument_keypair.pubkey(), true),
            AccountMeta::new(derived_key1, false),
            AccountMeta::new(derived_key2, false),
            AccountMeta::new_readonly(derived_key3, false),
            AccountMeta::new_readonly(system_program::id(), false),
            AccountMeta::new(from_keypair.pubkey(), true),
            AccountMeta::new_readonly(solana_sdk_ids::ed25519_program::id(), false),
            AccountMeta::new_readonly(invoke_program_id, false),
            AccountMeta::new_readonly(unexecutable_program_keypair.pubkey(), false),
        ];

        let do_invoke = |test: u8, additional_instructions: &[Instruction], bank: &Bank| {
            let instruction_data = &[test, bump_seed1, bump_seed2, bump_seed3];
            let signers = vec![
                &mint_keypair,
                &argument_keypair,
                &invoked_argument_keypair,
                &from_keypair,
            ];
            let mut instructions = vec![Instruction::new_with_bytes(
                invoke_program_id,
                instruction_data,
                account_metas.clone(),
            )];
            instructions.extend_from_slice(additional_instructions);
            let message = Message::new(&instructions, Some(&mint_pubkey));
            let tx = Transaction::new(&signers, message.clone(), bank.last_blockhash());
            let (result, inner_instructions, log_messages, executed_units) =
                process_transaction_and_record_inner(bank, tx);

            let invoked_programs: Vec<Pubkey> = inner_instructions
                .first()
                .map(|instructions| {
                    instructions
                        .iter()
                        .filter_map(|ix| {
                            message
                                .account_keys
                                .get(ix.instruction.program_id_index as usize)
                        })
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();

            let no_invoked_programs: Vec<Pubkey> = inner_instructions
                .get(1)
                .map(|instructions| {
                    instructions
                        .iter()
                        .filter_map(|ix| {
                            message
                                .account_keys
                                .get(ix.instruction.program_id_index as usize)
                        })
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();

            (
                result,
                log_messages,
                executed_units,
                invoked_programs,
                no_invoked_programs,
            )
        };

        // success cases

        let do_invoke_success = |test: u8,
                                 additional_instructions: &[Instruction],
                                 expected_invoked_programs: &[Pubkey],
                                 bank: &Bank| {
            println!("Running success test #{:?}", test);

            let (result, _log_messages, _executed_units, invoked_programs, no_invoked_programs) =
                do_invoke(test, additional_instructions, bank);

            assert_eq!(result, Ok(()));
            assert_eq!(invoked_programs.len(), expected_invoked_programs.len());
            assert_eq!(invoked_programs, expected_invoked_programs);
            assert_eq!(no_invoked_programs.len(), 0);
        };

        do_invoke_success(
            TEST_SUCCESS,
            &[Instruction::new_with_bytes(noop_program_id, &[], vec![])],
            match program.0 {
                Languages::C => vec![
                    system_program::id(),
                    system_program::id(),
                    invoked_program_id.clone(),
                    invoked_program_id.clone(),
                    invoked_program_id.clone(),
                    invoked_program_id.clone(),
                    invoked_program_id.clone(),
                    invoked_program_id.clone(),
                    invoked_program_id.clone(),
                    invoked_program_id.clone(),
                    invoked_program_id.clone(),
                    invoked_program_id.clone(),
                    invoked_program_id.clone(),
                    invoked_program_id.clone(),
                    invoked_program_id.clone(),
                    invoked_program_id.clone(),
                    invoked_program_id.clone(),
                    invoked_program_id.clone(),
                    invoked_program_id.clone(),
                ],
                Languages::Rust => vec![
                    system_program::id(),
                    system_program::id(),
                    invoked_program_id.clone(),
                    invoked_program_id.clone(),
                    invoked_program_id.clone(),
                    invoked_program_id.clone(),
                    invoked_program_id.clone(),
                    invoked_program_id.clone(),
                    invoked_program_id.clone(),
                    invoked_program_id.clone(),
                    invoked_program_id.clone(),
                    invoked_program_id.clone(),
                    invoked_program_id.clone(),
                    invoked_program_id.clone(),
                    invoked_program_id.clone(),
                    invoked_program_id.clone(),
                    invoked_program_id.clone(),
                    invoked_program_id.clone(),
                    invoked_program_id.clone(),
                    invoked_program_id.clone(),
                    invoked_program_id.clone(),
                    system_program::id(),
                    invoked_program_id.clone(),
                    invoked_program_id.clone(),
                ],
            }
            .as_ref(),
            &bank,
        );

        // With SIMD-0296 enabled, eight nested invokes should pass.
        let bank = bank_with_feature_activated(
            &bank_forks,
            bank,
            &feature_set::raise_cpi_nesting_limit_to_8::id(),
        );
        assert!(bank
            .feature_set
            .is_active(&feature_set::raise_cpi_nesting_limit_to_8::id()));
        {
            // Reset the account balances for `ARGUMENT` and `INVOKED_ARGUMENT`
            let account = AccountSharedData::new(42, 100, &invoke_program_id);
            bank.store_account(&argument_keypair.pubkey(), &account);

            let account = AccountSharedData::new(20, 10, &invoked_program_id);
            bank.store_account(&invoked_argument_keypair.pubkey(), &account);
        }
        do_invoke_success(
            TEST_NESTED_INVOKE_SIMD_0296_OK,
            &[],
            &[invoked_program_id.clone(); 16], // 16, 8 for each invoke
            &bank,
        );

        // failure cases

        let do_invoke_failure_test_local_with_compute_check =
            |test: u8,
             expected_error: TransactionError,
             expected_invoked_programs: &[Pubkey],
             expected_log_messages: Option<Vec<String>>,
             should_deplete_compute_meter: bool,
             bank: &Bank| {
                println!("Running failure test #{:?}", test);

                let compute_unit_limit = 1_000_000;
                let (result, log_messages, executed_units, invoked_programs, _) = do_invoke(
                    test,
                    &[ComputeBudgetInstruction::set_compute_unit_limit(
                        compute_unit_limit,
                    )],
                    bank,
                );

                assert_eq!(result, Err(expected_error));
                assert_eq!(invoked_programs, expected_invoked_programs);
                if should_deplete_compute_meter {
                    assert_eq!(executed_units, compute_unit_limit as u64);
                } else {
                    assert!(executed_units < compute_unit_limit as u64);
                }
                if let Some(expected_log_messages) = expected_log_messages {
                    assert_eq!(log_messages.len(), expected_log_messages.len());
                    expected_log_messages
                        .into_iter()
                        .zip(log_messages)
                        .for_each(|(expected_log_message, log_message)| {
                            if expected_log_message != String::from("skip") {
                                assert_eq!(log_message, expected_log_message);
                            }
                        });
                }
            };

        let do_invoke_failure_test_local =
            |test: u8,
             expected_error: TransactionError,
             expected_invoked_programs: &[Pubkey],
             expected_log_messages: Option<Vec<String>>,
             bank: &Bank| {
                do_invoke_failure_test_local_with_compute_check(
                    test,
                    expected_error,
                    expected_invoked_programs,
                    expected_log_messages,
                    false, // should_deplete_compute_meter
                    bank,
                )
            };

        let program_lang = match program.0 {
            Languages::Rust => "Rust",
            Languages::C => "C",
        };

        do_invoke_failure_test_local(
            TEST_PRIVILEGE_ESCALATION_SIGNER,
            TransactionError::InstructionError(0, InstructionError::PrivilegeEscalation),
            &[invoked_program_id.clone()],
            None,
            &bank,
        );

        do_invoke_failure_test_local(
            TEST_PRIVILEGE_ESCALATION_WRITABLE,
            TransactionError::InstructionError(0, InstructionError::PrivilegeEscalation),
            &[invoked_program_id.clone()],
            None,
            &bank,
        );

        do_invoke_failure_test_local(
            TEST_PPROGRAM_NOT_OWNED_BY_LOADER,
            TransactionError::InstructionError(0, InstructionError::UnsupportedProgramId),
            &[argument_keypair.pubkey()],
            None,
            &bank,
        );

        do_invoke_failure_test_local(
            TEST_PPROGRAM_NOT_EXECUTABLE,
            TransactionError::InstructionError(0, InstructionError::UnsupportedProgramId),
            &[unexecutable_program_keypair.pubkey()],
            None,
            &bank,
        );

        do_invoke_failure_test_local(
            TEST_EMPTY_ACCOUNTS_SLICE,
            TransactionError::InstructionError(0, InstructionError::MissingAccount),
            &[],
            None,
            &bank,
        );

        do_invoke_failure_test_local(
            TEST_CAP_SEEDS,
            TransactionError::InstructionError(0, InstructionError::MaxSeedLengthExceeded),
            &[],
            None,
            &bank,
        );

        do_invoke_failure_test_local(
            TEST_CAP_SIGNERS,
            TransactionError::InstructionError(0, InstructionError::ProgramFailedToComplete),
            &[],
            None,
            &bank,
        );

        do_invoke_failure_test_local(
            TEST_MAX_INSTRUCTION_DATA_LEN_EXCEEDED,
            TransactionError::InstructionError(0, InstructionError::ProgramFailedToComplete),
            &[],
            Some(vec![
                format!("Program {invoke_program_id} invoke [1]"),
                format!("Program log: invoke {program_lang} program"),
                "Program log: Test max instruction data len exceeded".into(),
                "skip".into(), // don't compare compute consumption logs
                format!("Program {invoke_program_id} failed: Invoked an instruction with data that is too large (10241 > 10240)"),
            ]),
            &bank,
        );

        do_invoke_failure_test_local(
            TEST_MAX_INSTRUCTION_ACCOUNTS_EXCEEDED,
            TransactionError::InstructionError(0, InstructionError::ProgramFailedToComplete),
            &[],
            Some(vec![
                format!("Program {invoke_program_id} invoke [1]"),
                format!("Program log: invoke {program_lang} program"),
                "Program log: Test max instruction accounts exceeded".into(),
                "skip".into(), // don't compare compute consumption logs
                format!("Program {invoke_program_id} failed: Invoked an instruction with too many accounts (256 > 255)"),
            ]),
            &bank,
        );

        do_invoke_failure_test_local(
            TEST_MAX_ACCOUNT_INFOS_EXCEEDED,
            TransactionError::InstructionError(0, InstructionError::ProgramFailedToComplete),
            &[],
            Some(vec![
                format!("Program {invoke_program_id} invoke [1]"),
                format!("Program log: invoke {program_lang} program"),
                "Program log: Test max account infos exceeded".into(),
                "skip".into(), // don't compare compute consumption logs
                format!("Program {invoke_program_id} failed: Invoked an instruction with too many account info's (129 > 128)"),
            ]),
            &bank,
        );

        do_invoke_failure_test_local(
            TEST_RETURN_ERROR,
            TransactionError::InstructionError(0, InstructionError::Custom(42)),
            &[invoked_program_id.clone()],
            None,
            &bank,
        );

        do_invoke_failure_test_local(
            TEST_PRIVILEGE_DEESCALATION_ESCALATION_SIGNER,
            TransactionError::InstructionError(0, InstructionError::PrivilegeEscalation),
            &[invoked_program_id.clone()],
            None,
            &bank,
        );

        do_invoke_failure_test_local(
            TEST_PRIVILEGE_DEESCALATION_ESCALATION_WRITABLE,
            TransactionError::InstructionError(0, InstructionError::PrivilegeEscalation),
            &[invoked_program_id.clone()],
            None,
            &bank,
        );

        do_invoke_failure_test_local_with_compute_check(
            TEST_WRITABLE_DEESCALATION_WRITABLE,
            TransactionError::InstructionError(0, InstructionError::ReadonlyDataModified),
            &[invoked_program_id.clone()],
            None,
            true, // should_deplete_compute_meter
            &bank,
        );

        // With SIMD-0296 disabled, five nested invokes is too deep.
        let bank = bank_with_feature_deactivated(
            &bank_forks,
            bank,
            &feature_set::raise_cpi_nesting_limit_to_8::id(),
        );
        assert!(!bank
            .feature_set
            .is_active(&feature_set::raise_cpi_nesting_limit_to_8::id()));
        do_invoke_failure_test_local(
            TEST_NESTED_INVOKE_TOO_DEEP,
            TransactionError::InstructionError(0, InstructionError::CallDepth),
            &[
                invoked_program_id.clone(),
                invoked_program_id.clone(),
                invoked_program_id.clone(),
                invoked_program_id.clone(),
                invoked_program_id.clone(),
            ],
            None,
            &bank,
        );

        // With SIMD-0296 enabled, nine nested invokes is too deep.
        let bank = bank_with_feature_activated(
            &bank_forks,
            bank,
            &feature_set::raise_cpi_nesting_limit_to_8::id(),
        );
        assert!(bank
            .feature_set
            .is_active(&feature_set::raise_cpi_nesting_limit_to_8::id()));
        do_invoke_failure_test_local(
            TEST_NESTED_INVOKE_SIMD_0296_TOO_DEEP,
            TransactionError::InstructionError(0, InstructionError::CallDepth),
            &[
                invoked_program_id.clone(),
                invoked_program_id.clone(),
                invoked_program_id.clone(),
                invoked_program_id.clone(),
                invoked_program_id.clone(),
                invoked_program_id.clone(),
                invoked_program_id.clone(),
                invoked_program_id.clone(),
                invoked_program_id.clone(),
            ],
            None,
            &bank,
        );

        do_invoke_failure_test_local(
            TEST_RETURN_DATA_TOO_LARGE,
            TransactionError::InstructionError(0, InstructionError::ProgramFailedToComplete),
            &[],
            None,
            &bank,
        );

        do_invoke_failure_test_local(
            TEST_DUPLICATE_PRIVILEGE_ESCALATION_SIGNER,
            TransactionError::InstructionError(0, InstructionError::PrivilegeEscalation),
            &[invoked_program_id.clone()],
            None,
            &bank,
        );

        do_invoke_failure_test_local(
            TEST_DUPLICATE_PRIVILEGE_ESCALATION_WRITABLE,
            TransactionError::InstructionError(0, InstructionError::PrivilegeEscalation),
            &[invoked_program_id.clone()],
            None,
            &bank,
        );

        // Check resulting state

        assert_eq!(43, bank.get_balance(&derived_key1));
        let account = bank.get_account(&derived_key1).unwrap();
        assert_eq!(&invoke_program_id, account.owner());
        assert_eq!(
            MAX_PERMITTED_DATA_INCREASE,
            bank.get_account(&derived_key1).unwrap().data().len()
        );
        for i in 0..20 {
            assert_eq!(i as u8, account.data()[i]);
        }

        // Attempt to realloc into unauthorized address space
        let account = AccountSharedData::new(84, 0, &system_program::id());
        bank.store_account(&from_keypair.pubkey(), &account);
        bank.store_account(&derived_key1, &AccountSharedData::default());
        let instruction = Instruction::new_with_bytes(
            invoke_program_id,
            &[
                TEST_ALLOC_ACCESS_VIOLATION,
                bump_seed1,
                bump_seed2,
                bump_seed3,
            ],
            account_metas.clone(),
        );
        let message = Message::new(&[instruction], Some(&mint_pubkey));
        let tx = Transaction::new(
            &[
                &mint_keypair,
                &argument_keypair,
                &invoked_argument_keypair,
                &from_keypair,
            ],
            message.clone(),
            bank.last_blockhash(),
        );
        let (result, inner_instructions, _log_messages, _executed_units) =
            process_transaction_and_record_inner(&bank, tx);
        let invoked_programs: Vec<Pubkey> = inner_instructions[0]
            .iter()
            .map(|ix| &message.account_keys[ix.instruction.program_id_index as usize])
            .cloned()
            .collect();
        assert_eq!(invoked_programs, vec![]);
        assert_eq!(
            result.unwrap_err(),
            TransactionError::InstructionError(0, InstructionError::ProgramFailedToComplete)
        );
    }
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_program_id_spoofing() {
    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50);

    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let mut bank_client = BankClient::new_shared(bank.clone());
    let authority_keypair = Keypair::new();

    let (_bank, malicious_swap_pubkey) = load_program_of_loader_v4(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_spoof1",
    );
    let (bank, malicious_system_pubkey) = load_program_of_loader_v4(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_spoof1_system",
    );

    let from_pubkey = Pubkey::new_unique();
    let account = AccountSharedData::new(10, 0, &system_program::id());
    bank.store_account(&from_pubkey, &account);

    let to_pubkey = Pubkey::new_unique();
    let account = AccountSharedData::new(0, 0, &system_program::id());
    bank.store_account(&to_pubkey, &account);

    let account_metas = vec![
        AccountMeta::new_readonly(system_program::id(), false),
        AccountMeta::new_readonly(malicious_system_pubkey, false),
        AccountMeta::new(from_pubkey, false),
        AccountMeta::new(to_pubkey, false),
    ];

    let instruction =
        Instruction::new_with_bytes(malicious_swap_pubkey, &[], account_metas.clone());
    let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
    assert_eq!(
        result.unwrap_err().unwrap(),
        TransactionError::InstructionError(0, InstructionError::MissingRequiredSignature)
    );
    assert_eq!(10, bank.get_balance(&from_pubkey));
    assert_eq!(0, bank.get_balance(&to_pubkey));
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_caller_has_access_to_cpi_program() {
    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50);

    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let mut bank_client = BankClient::new_shared(bank.clone());
    let authority_keypair = Keypair::new();

    let (_bank, caller_pubkey) = load_program_of_loader_v4(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_caller_access",
    );
    let (_bank, caller2_pubkey) = load_program_of_loader_v4(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_caller_access",
    );

    let account_metas = vec![
        AccountMeta::new_readonly(caller_pubkey, false),
        AccountMeta::new_readonly(caller2_pubkey, false),
    ];
    let instruction = Instruction::new_with_bytes(caller_pubkey, &[1], account_metas.clone());
    let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
    assert_eq!(
        result.unwrap_err().unwrap(),
        TransactionError::InstructionError(0, InstructionError::MissingAccount),
    );
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_ro_modify() {
    solana_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50);

    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let mut bank_client = BankClient::new_shared(bank.clone());
    let authority_keypair = Keypair::new();

    let (bank, program_pubkey) = load_program_of_loader_v4(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_ro_modify",
    );

    let test_keypair = Keypair::new();
    let account = AccountSharedData::new(10, 0, &system_program::id());
    bank.store_account(&test_keypair.pubkey(), &account);

    let account_metas = vec![
        AccountMeta::new_readonly(system_program::id(), false),
        AccountMeta::new(test_keypair.pubkey(), true),
    ];

    let instruction = Instruction::new_with_bytes(program_pubkey, &[1], account_metas.clone());
    let message = Message::new(&[instruction], Some(&mint_keypair.pubkey()));
    let result = bank_client.send_and_confirm_message(&[&mint_keypair, &test_keypair], message);
    assert_eq!(
        result.unwrap_err().unwrap(),
        TransactionError::InstructionError(0, InstructionError::ProgramFailedToComplete)
    );

    let instruction = Instruction::new_with_bytes(program_pubkey, &[3], account_metas.clone());
    let message = Message::new(&[instruction], Some(&mint_keypair.pubkey()));
    let result = bank_client.send_and_confirm_message(&[&mint_keypair, &test_keypair], message);
    assert_eq!(
        result.unwrap_err().unwrap(),
        TransactionError::InstructionError(0, InstructionError::ProgramFailedToComplete)
    );

    let instruction = Instruction::new_with_bytes(program_pubkey, &[4], account_metas.clone());
    let message = Message::new(&[instruction], Some(&mint_keypair.pubkey()));
    let result = bank_client.send_and_confirm_message(&[&mint_keypair, &test_keypair], message);
    assert_eq!(
        result.unwrap_err().unwrap(),
        TransactionError::InstructionError(0, InstructionError::ProgramFailedToComplete)
    );
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_call_depth() {
    solana_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50);

    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let mut bank_client = BankClient::new_shared(bank);
    let authority_keypair = Keypair::new();

    let (_bank, program_id) = load_program_of_loader_v4(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_call_depth",
    );

    let budget = ComputeBudget::new_with_defaults(
        genesis_config
            .accounts
            .contains_key(&feature_set::raise_cpi_nesting_limit_to_8::id()),
    );
    let instruction =
        Instruction::new_with_bincode(program_id, &(budget.max_call_depth - 1), vec![]);
    let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
    assert!(result.is_ok());

    let instruction = Instruction::new_with_bincode(program_id, &budget.max_call_depth, vec![]);
    let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
    assert!(result.is_err());
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_compute_budget() {
    solana_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50);

    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let mut bank_client = BankClient::new_shared(bank);
    let authority_keypair = Keypair::new();

    let (_bank, program_id) = load_program_of_loader_v4(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_noop",
    );
    let message = Message::new(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(150),
            Instruction::new_with_bincode(program_id, &0, vec![]),
        ],
        Some(&mint_keypair.pubkey()),
    );
    let result = bank_client.send_and_confirm_message(&[&mint_keypair], message);
    assert_eq!(
        result.unwrap_err().unwrap(),
        TransactionError::InstructionError(1, InstructionError::ProgramFailedToComplete),
    );
}

#[test]
fn assert_instruction_count() {
    solana_logger::setup();

    let mut programs = Vec::new();
    #[cfg(feature = "sbf_c")]
    {
        programs.extend_from_slice(&[
            ("alloc", 18572),
            ("sbf_to_sbf", 316),
            ("multiple_static", 210),
            ("noop", 6),
            ("noop++", 6),
            ("relative_call", 212),
            ("return_data", 1026),
            ("sanity", 2374),
            ("sanity++", 2274),
            ("secp256k1_recover", 25422),
            ("sha", 1446),
            ("struct_pass", 108),
            ("struct_ret", 122),
        ]);
    }
    #[cfg(feature = "sbf_rust")]
    {
        programs.extend_from_slice(&[
            ("solana_sbf_rust_128bit", 801),
            ("solana_sbf_rust_alloc", 4983),
            ("solana_sbf_rust_custom_heap", 303),
            ("solana_sbf_rust_dep_crate", 3),
            ("solana_sbf_rust_iter", 1414),
            ("solana_sbf_rust_many_args", 1287),
            ("solana_sbf_rust_mem", 1298),
            ("solana_sbf_rust_membuiltins", 330),
            ("solana_sbf_rust_noop", 313),
            ("solana_sbf_rust_param_passing", 109),
            ("solana_sbf_rust_rand", 276),
            ("solana_sbf_rust_sanity", 18116),
            ("solana_sbf_rust_secp256k1_recover", 89274),
            ("solana_sbf_rust_sha", 22811),
        ]);
    }

    println!("\n  {:36} expected actual  diff", "SBF program");
    for (program_name, expected_consumption) in programs.iter() {
        let loader_id = bpf_loader::id();
        let program_key = Pubkey::new_unique();
        let mut transaction_accounts = vec![
            (program_key, AccountSharedData::new(0, 0, &loader_id)),
            (
                Pubkey::new_unique(),
                AccountSharedData::new(0, 0, &program_key),
            ),
        ];
        let instruction_accounts = vec![AccountMeta {
            pubkey: transaction_accounts[1].0,
            is_signer: false,
            is_writable: false,
        }];
        transaction_accounts[0]
            .1
            .set_data_from_slice(&load_program_from_file(program_name));
        transaction_accounts[0].1.set_executable(true);

        let prev_compute_meter = RefCell::new(0);
        print!("  {:36} {:8}", program_name, *expected_consumption);
        mock_process_instruction(
            &loader_id,
            vec![0],
            &[],
            transaction_accounts,
            instruction_accounts,
            Ok(()),
            solana_bpf_loader_program::Entrypoint::vm,
            |invoke_context| {
                *prev_compute_meter.borrow_mut() = invoke_context.get_remaining();
                solana_bpf_loader_program::test_utils::load_all_invoked_programs(invoke_context);
            },
            |invoke_context| {
                let consumption = prev_compute_meter
                    .borrow()
                    .saturating_sub(invoke_context.get_remaining());
                let diff: i64 = consumption as i64 - *expected_consumption as i64;
                println!(
                    "{:6} {:+5} ({:+3.0}%)",
                    consumption,
                    diff,
                    100.0_f64 * consumption as f64 / *expected_consumption as f64 - 100.0_f64,
                );
                assert!(consumption <= *expected_consumption);
            },
        );
    }
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_instruction_introspection() {
    solana_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50_000);

    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let mut bank_client = BankClient::new_shared(bank.clone());
    let authority_keypair = Keypair::new();

    let (_bank, program_id) = load_program_of_loader_v4(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_instruction_introspection",
    );

    // Passing transaction
    let account_metas = vec![
        AccountMeta::new_readonly(program_id, false),
        AccountMeta::new_readonly(sysvar::instructions::id(), false),
    ];
    let instruction0 = Instruction::new_with_bytes(program_id, &[0u8, 0u8], account_metas.clone());
    let instruction1 = Instruction::new_with_bytes(program_id, &[0u8, 1u8], account_metas.clone());
    let instruction2 = Instruction::new_with_bytes(program_id, &[0u8, 2u8], account_metas);
    let message = Message::new(
        &[instruction0, instruction1, instruction2],
        Some(&mint_keypair.pubkey()),
    );
    let result = bank_client.send_and_confirm_message(&[&mint_keypair], message);
    assert!(result.is_ok());

    // writable special instructions11111 key, should not be allowed
    let account_metas = vec![AccountMeta::new(sysvar::instructions::id(), false)];
    let instruction = Instruction::new_with_bytes(program_id, &[0], account_metas);
    let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
    assert_eq!(
        result.unwrap_err().unwrap(),
        // sysvar write locks are demoted to read only. So this will no longer
        // cause InvalidAccountIndex error.
        TransactionError::InstructionError(0, InstructionError::ProgramFailedToComplete),
    );

    // No accounts, should error
    let instruction = Instruction::new_with_bytes(program_id, &[0], vec![]);
    let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
    assert!(result.is_err());
    assert_eq!(
        result.unwrap_err().unwrap(),
        TransactionError::InstructionError(0, InstructionError::NotEnoughAccountKeys)
    );
    assert!(bank.get_account(&sysvar::instructions::id()).is_none());
}

fn get_stable_genesis_config() -> GenesisConfigInfo {
    let validator_pubkey =
        Pubkey::from_str("GLh546CXmtZdvpEzL8sxzqhhUf7KPvmGaRpFHB5W1sjV").unwrap();
    let mint_keypair = Keypair::from_base58_string(
        "4YTH9JSRgZocmK9ezMZeJCCV2LVeR2NatTBA8AFXkg2x83fqrt8Vwyk91961E7ns4vee9yUBzuDfztb8i9iwTLFd",
    );
    let voting_keypair = Keypair::from_base58_string(
        "4EPWEn72zdNY1JSKkzyZ2vTZcKdPW3jM5WjAgUadnoz83FR5cDFApbo7s5mwBcYXn8afVe2syReJaqBi4fkhG3mH",
    );
    let stake_pubkey = Pubkey::from_str("HGq9JF77xFXRgWRJy8VQuhdbdugrT856RvQDzr1KJo6E").unwrap();

    let mut genesis_config = create_genesis_config_with_leader_ex(
        123,
        &mint_keypair.pubkey(),
        &validator_pubkey,
        &voting_keypair.pubkey(),
        &stake_pubkey,
        bootstrap_validator_stake_lamports(),
        42,
        FeeRateGovernor::new(0, 0), // most tests can't handle transaction fees
        Rent::free(),               // most tests don't expect rent
        ClusterType::Development,
        vec![],
    );
    genesis_config.creation_time = Duration::ZERO.as_secs() as UnixTimestamp;

    GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        voting_keypair,
        validator_pubkey,
    }
}

#[test]
#[ignore]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_invoke_stable_genesis_and_bank() {
    // The purpose of this test is to exercise various code branches of runtime/VM and
    // assert that the resulting bank hash matches with the expected value.
    // The assert check is commented out by default. Please refer to the last few lines
    // of the test to enable the assertion.
    solana_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = get_stable_genesis_config();
    let bank = Bank::new_for_tests(&genesis_config);
    let bank = Arc::new(bank);
    let bank_client = BankClient::new_shared(bank.clone());

    // Deploy upgradeable program
    let buffer_keypair = Keypair::from_base58_string(
        "4q4UvWxh2oMifTGbChDeWCbdN8eJEUQ1E6cuNnmymJ6AN5CMUT2VW5A1RKnG9dy7ypLczB9inMUAafh5TkpXrtxg",
    );
    let program_keypair = Keypair::from_base58_string(
        "3LQpBxgpaFNJPit5a8t51pJKMkUmNUn5PhSTcuuhuuBxe43cTeqVPhMtKkFNr5VpFzCExf4ihibvuZgGxmjy6t8n",
    );
    let program_id = program_keypair.pubkey();
    let authority_keypair = Keypair::from_base58_string(
        "285XFW2NTWd6CMvtHzvYYS1kWzmzcGBnyEXbH1v8hq6YJqJsLMTYMPkbEQqeE7m7UqhoMeK5V3HMJLf9DdxwU2Gy",
    );

    let instruction =
        Instruction::new_with_bytes(program_id, &[0], vec![AccountMeta::new(clock::id(), false)]);

    // Call program before its deployed
    let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction.clone());
    assert_eq!(
        result.unwrap_err().unwrap(),
        TransactionError::ProgramAccountNotFound
    );

    #[allow(deprecated)]
    solana_runtime::loader_utils::load_upgradeable_program(
        &bank_client,
        &mint_keypair,
        &buffer_keypair,
        &program_keypair,
        &authority_keypair,
        "solana_sbf_rust_noop",
    );

    // Deploy indirect invocation program
    let indirect_program_keypair = Keypair::from_base58_string(
        "2BgE4gD5wUCwiAVPYbmWd2xzXSsD9W2fWgNjwmVkm8WL7i51vK9XAXNnX1VB6oKQZmjaUPRd5RzE6RggB9DeKbZC",
    );
    #[allow(deprecated)]
    solana_runtime::loader_utils::load_upgradeable_program(
        &bank_client,
        &mint_keypair,
        &buffer_keypair,
        &indirect_program_keypair,
        &authority_keypair,
        "solana_sbf_rust_invoke_and_return",
    );

    let invoke_instruction =
        Instruction::new_with_bytes(program_id, &[0], vec![AccountMeta::new(clock::id(), false)]);
    let indirect_invoke_instruction = Instruction::new_with_bytes(
        indirect_program_keypair.pubkey(),
        &[0],
        vec![
            AccountMeta::new_readonly(program_id, false),
            AccountMeta::new_readonly(clock::id(), false),
        ],
    );

    // Prepare redeployment
    let buffer_keypair = Keypair::from_base58_string(
        "5T5L31FiUphXh4N6mxiWhEKPrdLhvMJSbaHo1Ne7zZYkw6YT1fVkqsWdA6pHMtqATiMTc4sfx5yTV9M9AnWDoBkW",
    );
    load_upgradeable_buffer(
        &bank_client,
        &mint_keypair,
        &buffer_keypair,
        &authority_keypair,
        "solana_sbf_rust_panic",
    );
    let redeployment_instruction = loader_v3_instruction::upgrade(
        &program_id,
        &buffer_keypair.pubkey(),
        &authority_keypair.pubkey(),
        &mint_keypair.pubkey(),
    );

    // Redeployment causes programs to be unavailable to both top-level-instructions and CPI instructions
    for invoke_instruction in [invoke_instruction, indirect_invoke_instruction] {
        // Call upgradeable program
        let result =
            bank_client.send_and_confirm_instruction(&mint_keypair, invoke_instruction.clone());
        assert!(result.is_ok());

        // Upgrade the program and invoke in same tx
        let message = Message::new(
            &[redeployment_instruction.clone(), invoke_instruction],
            Some(&mint_keypair.pubkey()),
        );
        let tx = Transaction::new(
            &[&mint_keypair, &authority_keypair],
            message.clone(),
            bank.last_blockhash(),
        );
        let (result, _, _, _) = process_transaction_and_record_inner(&bank, tx);
        assert_eq!(
            result.unwrap_err(),
            TransactionError::InstructionError(1, InstructionError::InvalidAccountData),
        );
    }

    // Prepare undeployment
    let (programdata_address, _) = Pubkey::find_program_address(
        &[program_keypair.pubkey().as_ref()],
        &bpf_loader_upgradeable::id(),
    );
    let undeployment_instruction = loader_v3_instruction::close_any(
        &programdata_address,
        &mint_keypair.pubkey(),
        Some(&authority_keypair.pubkey()),
        Some(&program_id),
    );

    let invoke_instruction =
        Instruction::new_with_bytes(program_id, &[1], vec![AccountMeta::new(clock::id(), false)]);
    let indirect_invoke_instruction = Instruction::new_with_bytes(
        indirect_program_keypair.pubkey(),
        &[1],
        vec![
            AccountMeta::new_readonly(program_id, false),
            AccountMeta::new_readonly(clock::id(), false),
        ],
    );

    // Undeployment is visible to both top-level-instructions and CPI instructions
    for invoke_instruction in [invoke_instruction, indirect_invoke_instruction] {
        // Call upgradeable program
        let result =
            bank_client.send_and_confirm_instruction(&mint_keypair, invoke_instruction.clone());
        assert!(result.is_ok());

        // Undeploy the program and invoke in same tx
        let message = Message::new(
            &[undeployment_instruction.clone(), invoke_instruction],
            Some(&mint_keypair.pubkey()),
        );
        let tx = Transaction::new(
            &[&mint_keypair, &authority_keypair],
            message.clone(),
            bank.last_blockhash(),
        );
        let (result, _, _, _) = process_transaction_and_record_inner(&bank, tx);
        assert_eq!(
            result.unwrap_err(),
            TransactionError::InstructionError(1, InstructionError::InvalidAccountData),
        );
    }

    bank.freeze();
    let expected_hash = Hash::from_str("2A2vqbUKExRbnaAzSnDFXdsBZRZSpCjGZCAA3mFZG2sV")
        .expect("Failed to generate hash");
    println!("Stable test produced bank hash: {}", bank.hash());
    println!("Expected hash: {}", expected_hash);

    // Enable the following code to match the bank hash with the expected bank hash.
    // Follow these steps.
    // 1. Run this test on the baseline/master commit, and get the expected bank hash.
    // 2. Update the `expected_hash` to match the expected bank hash.
    // 3. Run the test in the PR branch that's being tested.
    // If the hash doesn't match, the PR likely has runtime changes that can lead to
    // consensus failure.
    //  assert_eq!(bank.hash(), expected_hash);
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_invoke_in_same_tx_as_deployment() {
    solana_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50);
    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let mut bank_client = BankClient::new_shared(bank.clone());

    // Deploy upgradeable program
    let authority_keypair = Keypair::new();
    let (program_keypair, deployment_instructions) = instructions_to_load_program_of_loader_v4(
        &bank_client,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_noop",
        None,
        None,
    );
    let program_id = program_keypair.pubkey();

    // Deploy indirect invocation program
    let (bank, indirect_program_id) = load_program_of_loader_v4(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_invoke_and_return",
    );

    // Prepare invocations
    let invoke_instruction =
        Instruction::new_with_bytes(program_id, &[0], vec![AccountMeta::new(clock::id(), false)]);
    let indirect_invoke_instruction = Instruction::new_with_bytes(
        indirect_program_id,
        &[0],
        vec![
            AccountMeta::new_readonly(program_id, false),
            AccountMeta::new_readonly(clock::id(), false),
        ],
    );

    // Deployment is invisible to both top-level-instructions and CPI instructions
    for (index, invoke_instruction) in [invoke_instruction, indirect_invoke_instruction]
        .into_iter()
        .enumerate()
    {
        let mut instructions = deployment_instructions.clone();
        instructions.push(invoke_instruction);
        let tx = Transaction::new(
            &[&mint_keypair, &program_keypair, &authority_keypair],
            Message::new(&instructions, Some(&mint_keypair.pubkey())),
            bank.last_blockhash(),
        );
        if index == 0 {
            let result = load_execute_and_commit_transaction(&bank, tx);
            assert_eq!(
                result.unwrap().status,
                Err(TransactionError::ProgramAccountNotFound),
            );
        } else {
            let (result, _, _, _) = process_transaction_and_record_inner(&bank, tx);
            if let TransactionError::InstructionError(instr_no, ty) = result.unwrap_err() {
                // Asserting the instruction number as an upper bound, since the quantity of
                // instructions depends on the program size, which in turn depends on the SBPF
                // versions.
                assert!(instr_no <= 41);
                assert_eq!(ty, InstructionError::UnsupportedProgramId);
            } else {
                panic!("Invalid error type");
            }
        }
    }
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_invoke_in_same_tx_as_redeployment() {
    solana_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50);
    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let mut bank_client = BankClient::new_shared(bank.clone());

    // Deploy upgradeable program
    let authority_keypair = Keypair::new();
    let (_bank, program_id) = load_program_of_loader_v4(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_noop",
    );
    let (source_program_keypair, mut deployment_instructions) =
        instructions_to_load_program_of_loader_v4(
            &bank_client,
            &mint_keypair,
            &authority_keypair,
            "solana_sbf_rust_panic",
            None,
            Some(&program_id),
        );
    let undeployment_instruction =
        loader_v4_instruction::retract(&program_id, &authority_keypair.pubkey());
    let redeployment_instructions =
        deployment_instructions.split_off(deployment_instructions.len() - 3);
    let signers: &[&[&Keypair]] = &[
        &[&mint_keypair, &source_program_keypair],
        &[&mint_keypair, &authority_keypair],
    ];
    let signers = std::iter::once(signers[0]).chain(std::iter::repeat(signers[1]));
    for (instruction, signers) in deployment_instructions.into_iter().zip(signers) {
        let message = Message::new(&[instruction], Some(&mint_keypair.pubkey()));
        bank_client
            .send_and_confirm_message(signers, message)
            .unwrap();
    }

    // Deploy indirect invocation program
    let (bank, indirect_program_id) = load_program_of_loader_v4(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_invoke_and_return",
    );

    // Prepare invocations
    let invoke_instruction =
        Instruction::new_with_bytes(program_id, &[0], vec![AccountMeta::new(clock::id(), false)]);
    let indirect_invoke_instruction = Instruction::new_with_bytes(
        indirect_program_id,
        &[0],
        vec![
            AccountMeta::new_readonly(program_id, false),
            AccountMeta::new_readonly(clock::id(), false),
        ],
    );

    // Redeployment fails when top-level-instructions invoke the program because of write lock demotion
    // and the program becomes unavailable to CPI instructions
    for (invoke_instruction, expected_error) in [
        (
            invoke_instruction,
            TransactionError::InstructionError(0, InstructionError::InvalidArgument),
        ),
        (
            indirect_invoke_instruction,
            TransactionError::InstructionError(4, InstructionError::UnsupportedProgramId),
        ),
    ] {
        // Call upgradeable program
        let result =
            bank_client.send_and_confirm_instruction(&mint_keypair, invoke_instruction.clone());
        assert!(result.is_ok());

        // Upgrade the program and invoke in same tx
        let message = Message::new(
            &[
                undeployment_instruction.clone(),
                redeployment_instructions[0].clone(),
                redeployment_instructions[1].clone(),
                redeployment_instructions[2].clone(),
                invoke_instruction,
            ],
            Some(&mint_keypair.pubkey()),
        );
        let tx = Transaction::new(
            &[&mint_keypair, &authority_keypair],
            message.clone(),
            bank.last_blockhash(),
        );
        let (result, _, _, _) = process_transaction_and_record_inner(&bank, tx);
        assert_eq!(result.unwrap_err(), expected_error,);
    }
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_invoke_in_same_tx_as_undeployment() {
    solana_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50);
    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let mut bank_client = BankClient::new_shared(bank.clone());

    // Deploy upgradeable program
    let authority_keypair = Keypair::new();
    let (_bank, program_id) = load_program_of_loader_v4(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_noop",
    );

    // Deploy indirect invocation program
    let (bank, indirect_program_id) = load_program_of_loader_v4(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_invoke_and_return",
    );

    // Prepare invocations
    let invoke_instruction =
        Instruction::new_with_bytes(program_id, &[0], vec![AccountMeta::new(clock::id(), false)]);
    let indirect_invoke_instruction = Instruction::new_with_bytes(
        indirect_program_id,
        &[0],
        vec![
            AccountMeta::new_readonly(program_id, false),
            AccountMeta::new_readonly(clock::id(), false),
        ],
    );

    // Prepare undeployment
    let undeployment_instruction =
        loader_v4_instruction::retract(&program_id, &authority_keypair.pubkey());

    // Undeployment fails when top-level-instructions invoke the program because of write lock demotion
    // and the program becomes unavailable to CPI instructions
    for (invoke_instruction, expected_error) in [
        (
            invoke_instruction,
            TransactionError::InstructionError(0, InstructionError::InvalidArgument),
        ),
        (
            indirect_invoke_instruction,
            TransactionError::InstructionError(1, InstructionError::UnsupportedProgramId),
        ),
    ] {
        // Call upgradeable program
        let result =
            bank_client.send_and_confirm_instruction(&mint_keypair, invoke_instruction.clone());
        assert!(result.is_ok());

        // Upgrade the program and invoke in same tx
        let message = Message::new(
            &[undeployment_instruction.clone(), invoke_instruction],
            Some(&mint_keypair.pubkey()),
        );
        let tx = Transaction::new(
            &[&mint_keypair, &authority_keypair],
            message.clone(),
            bank.last_blockhash(),
        );
        let (result, _, _, _) = process_transaction_and_record_inner(&bank, tx);
        assert_eq!(result.unwrap_err(), expected_error,);
    }
}

#[test]
#[cfg(any(feature = "sbf_c", feature = "sbf_rust"))]
fn test_program_sbf_disguised_as_sbf_loader() {
    solana_logger::setup();

    let mut programs = Vec::new();
    #[cfg(feature = "sbf_c")]
    {
        programs.extend_from_slice(&[("noop")]);
    }
    #[cfg(feature = "sbf_rust")]
    {
        programs.extend_from_slice(&[("solana_sbf_rust_noop")]);
    }

    for program in programs.iter() {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(50);
        let mut bank = Bank::new_for_tests(&genesis_config);
        bank.deactivate_feature(&agave_feature_set::remove_bpf_loader_incorrect_program_id::id());
        let (bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();
        let mut bank_client = BankClient::new_shared(bank);
        let authority_keypair = Keypair::new();

        let (_bank, program_id) = load_program_of_loader_v4(
            &mut bank_client,
            &bank_forks,
            &mint_keypair,
            &authority_keypair,
            program,
        );

        let account_metas = vec![AccountMeta::new_readonly(program_id, false)];
        let instruction = Instruction::new_with_bytes(bpf_loader::id(), &[1], account_metas);
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        assert_eq!(
            result.unwrap_err().unwrap(),
            TransactionError::InstructionError(0, InstructionError::UnsupportedProgramId)
        );
    }
}

#[test]
#[cfg(feature = "sbf_c")]
fn test_program_reads_from_program_account() {
    use solana_loader_v4_interface::state as loader_v4_state;
    solana_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50);

    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let mut bank_client = BankClient::new_shared(bank);
    let authority_keypair = Keypair::new();

    let (_bank, program_id) = load_program_of_loader_v4(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "read_program",
    );
    let data = bank_client.get_account_data(&program_id).unwrap().unwrap();
    let account_metas = vec![AccountMeta::new_readonly(program_id, false)];
    let instruction = Instruction::new_with_bytes(
        program_id,
        &data[0..loader_v4_state::LoaderV4State::program_data_offset()],
        account_metas,
    );
    bank_client
        .send_and_confirm_instruction(&mint_keypair, instruction)
        .unwrap();
}

#[test]
#[cfg(feature = "sbf_c")]
fn test_program_sbf_c_dup() {
    solana_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50);

    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

    let account_address = Pubkey::new_unique();
    let account = AccountSharedData::new_data(42, &[1_u8, 2, 3], &system_program::id()).unwrap();
    bank.store_account(&account_address, &account);

    let mut bank_client = BankClient::new_shared(bank);
    let authority_keypair = Keypair::new();
    let (_bank, program_id) = load_program_of_loader_v4(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "ser",
    );
    let account_metas = vec![
        AccountMeta::new_readonly(account_address, false),
        AccountMeta::new_readonly(account_address, false),
    ];
    let instruction = Instruction::new_with_bytes(program_id, &[4, 5, 6, 7], account_metas);
    bank_client
        .send_and_confirm_instruction(&mint_keypair, instruction)
        .unwrap();
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_upgrade() {
    solana_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50);
    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let mut bank_client = BankClient::new_shared(bank);

    // Deploy upgrade program
    let authority_keypair = Keypair::new();
    let (_bank, program_id) = load_program_of_loader_v4(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_upgradeable",
    );

    // Call upgradeable program
    let mut instruction =
        Instruction::new_with_bytes(program_id, &[0], vec![AccountMeta::new(clock::id(), false)]);
    let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction.clone());
    assert_eq!(
        result.unwrap_err().unwrap(),
        TransactionError::InstructionError(0, InstructionError::Custom(42))
    );

    // Set authority
    let new_authority_keypair = Keypair::new();
    let authority_instruction = loader_v4_instruction::transfer_authority(
        &program_id,
        &authority_keypair.pubkey(),
        &new_authority_keypair.pubkey(),
    );
    let message = Message::new(&[authority_instruction], Some(&mint_keypair.pubkey()));
    bank_client
        .send_and_confirm_message(
            &[&mint_keypair, &authority_keypair, &new_authority_keypair],
            message,
        )
        .unwrap();

    // Upgrade program
    let (source_program_keypair, mut deployment_instructions) =
        instructions_to_load_program_of_loader_v4(
            &bank_client,
            &mint_keypair,
            &new_authority_keypair,
            "solana_sbf_rust_upgraded",
            None,
            Some(&program_id),
        );
    deployment_instructions.insert(
        deployment_instructions.len() - 3,
        loader_v4_instruction::retract(&program_id, &new_authority_keypair.pubkey()),
    );
    let signers: &[&[&Keypair]] = &[
        &[&mint_keypair, &source_program_keypair],
        &[&mint_keypair, &new_authority_keypair],
    ];
    let signers = std::iter::once(signers[0]).chain(std::iter::repeat(signers[1]));
    for (instruction, signers) in deployment_instructions.into_iter().zip(signers) {
        let message = Message::new(&[instruction], Some(&mint_keypair.pubkey()));
        bank_client
            .send_and_confirm_message(signers, message)
            .unwrap();
    }
    bank_client
        .advance_slot(1, &bank_forks, &Pubkey::default())
        .expect("Failed to advance the slot");

    // Call upgraded program
    instruction.data[0] += 1;
    let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction.clone());
    assert_eq!(
        result.unwrap_err().unwrap(),
        TransactionError::InstructionError(0, InstructionError::Custom(43))
    );
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_upgrade_via_cpi() {
    solana_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50);

    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let mut bank_client = BankClient::new_shared(bank);
    let authority_keypair = Keypair::new();

    let (_bank, invoke_and_return) = load_program_of_loader_v4(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_invoke_and_return",
    );

    // Deploy upgradeable program
    let authority_keypair = Keypair::new();
    let (_bank, program_id) = load_program_of_loader_v4(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_upgradeable",
    );

    // Call the upgradable program via CPI
    let mut instruction = Instruction::new_with_bytes(
        invoke_and_return,
        &[0],
        vec![
            AccountMeta::new_readonly(program_id, false),
            AccountMeta::new_readonly(clock::id(), false),
        ],
    );
    instruction.data[0] += 1;
    let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction.clone());
    assert_eq!(
        result.unwrap_err().unwrap(),
        TransactionError::InstructionError(0, InstructionError::Custom(42))
    );

    // Set authority via CPI
    let new_authority_keypair = Keypair::new();
    let mut authority_instruction = loader_v4_instruction::transfer_authority(
        &program_id,
        &authority_keypair.pubkey(),
        &new_authority_keypair.pubkey(),
    );
    authority_instruction.program_id = invoke_and_return;
    authority_instruction
        .accounts
        .insert(0, AccountMeta::new(loader_v4::id(), false));
    let message = Message::new(&[authority_instruction], Some(&mint_keypair.pubkey()));
    bank_client
        .send_and_confirm_message(
            &[&mint_keypair, &authority_keypair, &new_authority_keypair],
            message,
        )
        .unwrap();

    // Upgrade program via CPI
    let (source_program_keypair, mut deployment_instructions) =
        instructions_to_load_program_of_loader_v4(
            &bank_client,
            &mint_keypair,
            &new_authority_keypair,
            "solana_sbf_rust_upgraded",
            None,
            Some(&program_id),
        );
    deployment_instructions.insert(
        deployment_instructions.len() - 3,
        loader_v4_instruction::retract(&program_id, &new_authority_keypair.pubkey()),
    );
    let mut upgrade_instruction = deployment_instructions.pop().unwrap();
    let signers: &[&[&Keypair]] = &[
        &[&mint_keypair, &source_program_keypair],
        &[&mint_keypair, &new_authority_keypair],
    ];
    let signers = std::iter::once(signers[0]).chain(std::iter::repeat(signers[1]));
    for (instruction, signers) in deployment_instructions.into_iter().zip(signers) {
        let message = Message::new(&[instruction], Some(&mint_keypair.pubkey()));
        bank_client
            .send_and_confirm_message(signers, message)
            .unwrap();
    }
    upgrade_instruction.program_id = invoke_and_return;
    upgrade_instruction
        .accounts
        .insert(0, AccountMeta::new(loader_v4::id(), false));
    let message = Message::new(&[upgrade_instruction], Some(&mint_keypair.pubkey()));
    bank_client
        .send_and_confirm_message(&[&mint_keypair, &new_authority_keypair], message)
        .unwrap();
    bank_client
        .advance_slot(1, &bank_forks, &Pubkey::default())
        .expect("Failed to advance the slot");

    // Call the upgraded program via CPI
    instruction.data[0] += 1;
    let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction.clone());
    assert_eq!(
        result.unwrap_err().unwrap(),
        TransactionError::InstructionError(0, InstructionError::Custom(43))
    );
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_ro_account_modify() {
    solana_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50);

    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let mut bank_client = BankClient::new_shared(bank.clone());
    let authority_keypair = Keypair::new();

    let (bank, program_id) = load_program_of_loader_v4(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_ro_account_modify",
    );

    let argument_keypair = Keypair::new();
    let account = AccountSharedData::new(42, 100, &program_id);
    bank.store_account(&argument_keypair.pubkey(), &account);

    let from_keypair = Keypair::new();
    let account = AccountSharedData::new(84, 0, &system_program::id());
    bank.store_account(&from_keypair.pubkey(), &account);

    let mint_pubkey = mint_keypair.pubkey();
    let account_metas = vec![
        AccountMeta::new_readonly(argument_keypair.pubkey(), false),
        AccountMeta::new_readonly(program_id, false),
    ];

    let instruction = Instruction::new_with_bytes(program_id, &[0], account_metas.clone());
    let message = Message::new(&[instruction], Some(&mint_pubkey));
    let result = bank_client.send_and_confirm_message(&[&mint_keypair], message);
    assert_eq!(
        result.unwrap_err().unwrap(),
        TransactionError::InstructionError(0, InstructionError::ReadonlyDataModified)
    );

    let instruction = Instruction::new_with_bytes(program_id, &[1], account_metas.clone());
    let message = Message::new(&[instruction], Some(&mint_pubkey));
    let result = bank_client.send_and_confirm_message(&[&mint_keypair], message);
    assert_eq!(
        result.unwrap_err().unwrap(),
        TransactionError::InstructionError(0, InstructionError::ReadonlyDataModified)
    );

    let instruction = Instruction::new_with_bytes(program_id, &[2], account_metas.clone());
    let message = Message::new(&[instruction], Some(&mint_pubkey));
    let result = bank_client.send_and_confirm_message(&[&mint_keypair], message);
    assert_eq!(
        result.unwrap_err().unwrap(),
        TransactionError::InstructionError(0, InstructionError::ReadonlyDataModified)
    );
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_realloc() {
    solana_logger::setup();

    const START_BALANCE: u64 = 100_000_000_000;

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(1_000_000_000_000);

    let mint_pubkey = mint_keypair.pubkey();
    let signer = &[&mint_keypair];
    for direct_mapping in [false, true] {
        let mut bank = Bank::new_for_tests(&genesis_config);
        let feature_set = Arc::make_mut(&mut bank.feature_set);
        // by default test banks have all features enabled, so we only need to
        // disable when needed
        if !direct_mapping {
            feature_set.deactivate(&feature_set::bpf_account_data_direct_mapping::id());
        }
        let (bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();
        let mut bank_client = BankClient::new_shared(bank.clone());
        let authority_keypair = Keypair::new();

        let (bank, program_id) = load_program_of_loader_v4(
            &mut bank_client,
            &bank_forks,
            &mint_keypair,
            &authority_keypair,
            "solana_sbf_rust_realloc",
        );

        let mut bump = 0;
        let keypair = Keypair::new();
        let pubkey = keypair.pubkey();
        let account = AccountSharedData::new(START_BALANCE, 5, &program_id);
        bank.store_account(&pubkey, &account);

        // Realloc RO account
        let mut instruction = realloc(&program_id, &pubkey, 0, &mut bump);
        instruction.accounts[0].is_writable = false;
        assert_eq!(
            bank_client
                .send_and_confirm_message(
                    signer,
                    Message::new(
                        &[
                            instruction,
                            ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                                LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST
                            ),
                        ],
                        Some(&mint_pubkey),
                    ),
                )
                .unwrap_err()
                .unwrap(),
            TransactionError::InstructionError(0, InstructionError::ReadonlyDataModified)
        );

        // Realloc account to overflow
        assert_eq!(
            bank_client
                .send_and_confirm_message(
                    signer,
                    Message::new(
                        &[
                            realloc(&program_id, &pubkey, usize::MAX, &mut bump),
                            ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                                LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST
                            ),
                        ],
                        Some(&mint_pubkey),
                    ),
                )
                .unwrap_err()
                .unwrap(),
            TransactionError::InstructionError(0, InstructionError::InvalidRealloc)
        );

        // Realloc account to 0
        bank_client
            .send_and_confirm_message(
                signer,
                Message::new(
                    &[
                        realloc(&program_id, &pubkey, 0, &mut bump),
                        ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                            LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST,
                        ),
                    ],
                    Some(&mint_pubkey),
                ),
            )
            .unwrap();
        let data = bank_client.get_account_data(&pubkey).unwrap().unwrap();
        assert_eq!(0, data.len());

        // Realloc account to max then undo
        bank_client
            .send_and_confirm_message(
                signer,
                Message::new(
                    &[
                        realloc_extend_and_undo(
                            &program_id,
                            &pubkey,
                            MAX_PERMITTED_DATA_INCREASE,
                            &mut bump,
                        ),
                        ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                            LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST,
                        ),
                    ],
                    Some(&mint_pubkey),
                ),
            )
            .unwrap();
        let data = bank_client.get_account_data(&pubkey).unwrap().unwrap();
        assert_eq!(0, data.len());

        // Realloc account to max + 1 then undo
        assert_eq!(
            bank_client
                .send_and_confirm_message(
                    signer,
                    Message::new(
                        &[
                            realloc_extend_and_undo(
                                &program_id,
                                &pubkey,
                                MAX_PERMITTED_DATA_INCREASE + 1,
                                &mut bump,
                            ),
                            ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                                LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST
                            ),
                        ],
                        Some(&mint_pubkey),
                    ),
                )
                .unwrap_err()
                .unwrap(),
            TransactionError::InstructionError(0, InstructionError::InvalidRealloc)
        );

        // Realloc to max + 1
        assert_eq!(
            bank_client
                .send_and_confirm_message(
                    signer,
                    Message::new(
                        &[
                            realloc(
                                &program_id,
                                &pubkey,
                                MAX_PERMITTED_DATA_INCREASE + 1,
                                &mut bump
                            ),
                            ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                                LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST
                            ),
                        ],
                        Some(&mint_pubkey),
                    ),
                )
                .unwrap_err()
                .unwrap(),
            TransactionError::InstructionError(0, InstructionError::InvalidRealloc)
        );

        // Realloc to max length in max increase increments
        for i in 0..MAX_PERMITTED_DATA_LENGTH as usize / MAX_PERMITTED_DATA_INCREASE {
            let mut bump = i as u64;
            bank_client
                .send_and_confirm_message(
                    signer,
                    Message::new(
                        &[
                            realloc_extend_and_fill(
                                &program_id,
                                &pubkey,
                                MAX_PERMITTED_DATA_INCREASE,
                                1,
                                &mut bump,
                            ),
                            ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                                LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST,
                            ),
                        ],
                        Some(&mint_pubkey),
                    ),
                )
                .unwrap();
            let data = bank_client.get_account_data(&pubkey).unwrap().unwrap();
            assert_eq!((i + 1) * MAX_PERMITTED_DATA_INCREASE, data.len());
        }
        for i in 0..data.len() {
            assert_eq!(data[i], 1);
        }

        // and one more time should fail
        assert_eq!(
            bank_client
                .send_and_confirm_message(
                    signer,
                    Message::new(
                        &[
                            realloc_extend(
                                &program_id,
                                &pubkey,
                                MAX_PERMITTED_DATA_INCREASE,
                                &mut bump
                            ),
                            ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                                LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST
                            ),
                        ],
                        Some(&mint_pubkey),
                    )
                )
                .unwrap_err()
                .unwrap(),
            TransactionError::InstructionError(0, InstructionError::InvalidRealloc)
        );

        // Realloc to 6 bytes
        bank_client
            .send_and_confirm_message(
                signer,
                Message::new(
                    &[
                        realloc(&program_id, &pubkey, 6, &mut bump),
                        ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                            LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST,
                        ),
                    ],
                    Some(&mint_pubkey),
                ),
            )
            .unwrap();
        let data = bank_client.get_account_data(&pubkey).unwrap().unwrap();
        assert_eq!(6, data.len());

        // Extend by 2 bytes and write a u64. This ensures that we can do writes that span the original
        // account length (6 bytes) and the realloc data (2 bytes).
        bank_client
            .send_and_confirm_message(
                signer,
                Message::new(
                    &[
                        extend_and_write_u64(&program_id, &pubkey, 0x1122334455667788),
                        ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                            LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST,
                        ),
                    ],
                    Some(&mint_pubkey),
                ),
            )
            .unwrap();
        let data = bank_client.get_account_data(&pubkey).unwrap().unwrap();
        assert_eq!(8, data.len());
        assert_eq!(0x1122334455667788, unsafe { *data.as_ptr().cast::<u64>() });

        // Realloc to 0
        bank_client
            .send_and_confirm_message(
                signer,
                Message::new(
                    &[
                        realloc(&program_id, &pubkey, 0, &mut bump),
                        ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                            LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST,
                        ),
                    ],
                    Some(&mint_pubkey),
                ),
            )
            .unwrap();
        let data = bank_client.get_account_data(&pubkey).unwrap().unwrap();
        assert_eq!(0, data.len());

        // Realloc and assign
        bank_client
            .send_and_confirm_message(
                signer,
                Message::new(
                    &[
                        Instruction::new_with_bytes(
                            program_id,
                            &[REALLOC_AND_ASSIGN],
                            vec![AccountMeta::new(pubkey, false)],
                        ),
                        ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                            LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST,
                        ),
                    ],
                    Some(&mint_pubkey),
                ),
            )
            .unwrap();
        let account = bank.get_account(&pubkey).unwrap();
        assert_eq!(&solana_system_interface::program::id(), account.owner());
        let data = bank_client.get_account_data(&pubkey).unwrap().unwrap();
        assert_eq!(MAX_PERMITTED_DATA_INCREASE, data.len());

        // Realloc to 0 with wrong owner
        assert_eq!(
            bank_client
                .send_and_confirm_message(
                    signer,
                    Message::new(
                        &[
                            realloc(&program_id, &pubkey, 0, &mut bump),
                            ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                                LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST
                            ),
                        ],
                        Some(&mint_pubkey),
                    ),
                )
                .unwrap_err()
                .unwrap(),
            TransactionError::InstructionError(0, InstructionError::AccountDataSizeChanged)
        );

        // realloc and assign to self via cpi
        assert_eq!(
            bank_client
                .send_and_confirm_message(
                    &[&mint_keypair, &keypair],
                    Message::new(
                        &[
                            Instruction::new_with_bytes(
                                program_id,
                                &[REALLOC_AND_ASSIGN_TO_SELF_VIA_SYSTEM_PROGRAM],
                                vec![
                                    AccountMeta::new(pubkey, true),
                                    AccountMeta::new(solana_system_interface::program::id(), false),
                                ],
                            ),
                            ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                                LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST
                            ),
                        ],
                        Some(&mint_pubkey),
                    )
                )
                .unwrap_err()
                .unwrap(),
            TransactionError::InstructionError(0, InstructionError::AccountDataSizeChanged)
        );

        // Assign to self and realloc via cpi
        bank_client
            .send_and_confirm_message(
                &[&mint_keypair, &keypair],
                Message::new(
                    &[
                        Instruction::new_with_bytes(
                            program_id,
                            &[ASSIGN_TO_SELF_VIA_SYSTEM_PROGRAM_AND_REALLOC],
                            vec![
                                AccountMeta::new(pubkey, true),
                                AccountMeta::new(solana_system_interface::program::id(), false),
                            ],
                        ),
                        ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                            LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST,
                        ),
                    ],
                    Some(&mint_pubkey),
                ),
            )
            .unwrap();
        let account = bank.get_account(&pubkey).unwrap();
        assert_eq!(&program_id, account.owner());
        let data = bank_client.get_account_data(&pubkey).unwrap().unwrap();
        assert_eq!(2 * MAX_PERMITTED_DATA_INCREASE, data.len());

        // Realloc to 0
        bank_client
            .send_and_confirm_message(
                signer,
                Message::new(
                    &[
                        realloc(&program_id, &pubkey, 0, &mut bump),
                        ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                            LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST,
                        ),
                    ],
                    Some(&mint_pubkey),
                ),
            )
            .unwrap();
        let data = bank_client.get_account_data(&pubkey).unwrap().unwrap();
        assert_eq!(0, data.len());

        // zero-init
        bank_client
            .send_and_confirm_message(
                &[&mint_keypair, &keypair],
                Message::new(
                    &[
                        Instruction::new_with_bytes(
                            program_id,
                            &[ZERO_INIT],
                            vec![AccountMeta::new(pubkey, true)],
                        ),
                        ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                            LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST,
                        ),
                    ],
                    Some(&mint_pubkey),
                ),
            )
            .unwrap();
    }
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_realloc_invoke() {
    solana_logger::setup();

    const START_BALANCE: u64 = 100_000_000_000;

    let GenesisConfigInfo {
        mut genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(1_000_000_000_000);
    genesis_config.rent = Rent::default();

    let mint_pubkey = mint_keypair.pubkey();
    let signer = &[&mint_keypair];

    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let mut bank_client = BankClient::new_shared(bank.clone());
    let authority_keypair = Keypair::new();

    let (_bank, realloc_program_id) = load_program_of_loader_v4(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_realloc",
    );
    let (bank, realloc_invoke_program_id) = load_program_of_loader_v4(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_realloc_invoke",
    );

    let mut bump = 0;
    let keypair = Keypair::new();
    let pubkey = keypair.pubkey().clone();
    let account = AccountSharedData::new(START_BALANCE, 5, &realloc_program_id);
    bank.store_account(&pubkey, &account);
    let invoke_keypair = Keypair::new();
    let invoke_pubkey = invoke_keypair.pubkey().clone();

    // Realloc RO account
    assert_eq!(
        bank_client
            .send_and_confirm_message(
                signer,
                Message::new(
                    &[
                        Instruction::new_with_bytes(
                            realloc_invoke_program_id,
                            &[INVOKE_REALLOC_ZERO_RO],
                            vec![
                                AccountMeta::new_readonly(pubkey, false),
                                AccountMeta::new_readonly(realloc_program_id, false),
                            ],
                        ),
                        ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                            LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST
                        ),
                    ],
                    Some(&mint_pubkey),
                )
            )
            .unwrap_err()
            .unwrap(),
        TransactionError::InstructionError(0, InstructionError::ReadonlyDataModified)
    );
    let account = bank.get_account(&pubkey).unwrap();
    assert_eq!(account.lamports(), START_BALANCE);

    // Realloc account to 0
    bank_client
        .send_and_confirm_message(
            signer,
            Message::new(
                &[
                    realloc(&realloc_program_id, &pubkey, 0, &mut bump),
                    ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                        LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST,
                    ),
                ],
                Some(&mint_pubkey),
            ),
        )
        .unwrap();
    let account = bank.get_account(&pubkey).unwrap();
    assert_eq!(account.lamports(), START_BALANCE);
    let data = bank_client.get_account_data(&pubkey).unwrap().unwrap();
    assert_eq!(0, data.len());

    // Realloc to max + 1
    assert_eq!(
        bank_client
            .send_and_confirm_message(
                signer,
                Message::new(
                    &[
                        Instruction::new_with_bytes(
                            realloc_invoke_program_id,
                            &[INVOKE_REALLOC_MAX_PLUS_ONE],
                            vec![
                                AccountMeta::new(pubkey, false),
                                AccountMeta::new_readonly(realloc_program_id, false),
                            ],
                        ),
                        ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                            LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST
                        ),
                    ],
                    Some(&mint_pubkey),
                )
            )
            .unwrap_err()
            .unwrap(),
        TransactionError::InstructionError(0, InstructionError::InvalidRealloc)
    );

    // Realloc to max twice
    assert_eq!(
        bank_client
            .send_and_confirm_message(
                signer,
                Message::new(
                    &[
                        Instruction::new_with_bytes(
                            realloc_invoke_program_id,
                            &[INVOKE_REALLOC_MAX_TWICE],
                            vec![
                                AccountMeta::new(pubkey, false),
                                AccountMeta::new_readonly(realloc_program_id, false),
                            ],
                        ),
                        ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                            LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST
                        ),
                    ],
                    Some(&mint_pubkey),
                )
            )
            .unwrap_err()
            .unwrap(),
        TransactionError::InstructionError(0, InstructionError::InvalidRealloc)
    );

    // Realloc account to 0
    bank_client
        .send_and_confirm_message(
            signer,
            Message::new(
                &[
                    realloc(&realloc_program_id, &pubkey, 0, &mut bump),
                    ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                        LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST,
                    ),
                ],
                Some(&mint_pubkey),
            ),
        )
        .unwrap();
    let account = bank.get_account(&pubkey).unwrap();
    assert_eq!(account.lamports(), START_BALANCE);
    let data = bank_client.get_account_data(&pubkey).unwrap().unwrap();
    assert_eq!(0, data.len());

    // Realloc and assign
    bank_client
        .send_and_confirm_message(
            signer,
            Message::new(
                &[
                    Instruction::new_with_bytes(
                        realloc_invoke_program_id,
                        &[INVOKE_REALLOC_AND_ASSIGN],
                        vec![
                            AccountMeta::new(pubkey, false),
                            AccountMeta::new_readonly(realloc_program_id, false),
                        ],
                    ),
                    ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                        LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST,
                    ),
                ],
                Some(&mint_pubkey),
            ),
        )
        .unwrap();
    let account = bank.get_account(&pubkey).unwrap();
    assert_eq!(&solana_system_interface::program::id(), account.owner());
    let data = bank_client.get_account_data(&pubkey).unwrap().unwrap();
    assert_eq!(MAX_PERMITTED_DATA_INCREASE, data.len());

    // Realloc to 0 with wrong owner
    assert_eq!(
        bank_client
            .send_and_confirm_message(
                signer,
                Message::new(
                    &[
                        realloc(&realloc_program_id, &pubkey, 0, &mut bump),
                        ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                            LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST
                        ),
                    ],
                    Some(&mint_pubkey),
                )
            )
            .unwrap_err()
            .unwrap(),
        TransactionError::InstructionError(0, InstructionError::AccountDataSizeChanged)
    );

    // realloc and assign to self via system program
    assert_eq!(
        bank_client
            .send_and_confirm_message(
                &[&mint_keypair, &keypair],
                Message::new(
                    &[
                        Instruction::new_with_bytes(
                            realloc_invoke_program_id,
                            &[INVOKE_REALLOC_AND_ASSIGN_TO_SELF_VIA_SYSTEM_PROGRAM],
                            vec![
                                AccountMeta::new(pubkey, true),
                                AccountMeta::new_readonly(realloc_program_id, false),
                                AccountMeta::new_readonly(
                                    solana_system_interface::program::id(),
                                    false
                                ),
                            ],
                        ),
                        ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                            LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST
                        ),
                    ],
                    Some(&mint_pubkey),
                )
            )
            .unwrap_err()
            .unwrap(),
        TransactionError::InstructionError(0, InstructionError::AccountDataSizeChanged)
    );

    // Assign to self and realloc via system program
    bank_client
        .send_and_confirm_message(
            &[&mint_keypair, &keypair],
            Message::new(
                &[
                    Instruction::new_with_bytes(
                        realloc_invoke_program_id,
                        &[INVOKE_ASSIGN_TO_SELF_VIA_SYSTEM_PROGRAM_AND_REALLOC],
                        vec![
                            AccountMeta::new(pubkey, true),
                            AccountMeta::new_readonly(realloc_program_id, false),
                            AccountMeta::new_readonly(
                                solana_system_interface::program::id(),
                                false,
                            ),
                        ],
                    ),
                    ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                        LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST,
                    ),
                ],
                Some(&mint_pubkey),
            ),
        )
        .unwrap();
    let account = bank.get_account(&pubkey).unwrap();
    assert_eq!(&realloc_program_id, account.owner());
    let data = bank_client.get_account_data(&pubkey).unwrap().unwrap();
    assert_eq!(2 * MAX_PERMITTED_DATA_INCREASE, data.len());

    // Realloc to 0
    bank_client
        .send_and_confirm_message(
            signer,
            Message::new(
                &[
                    realloc(&realloc_program_id, &pubkey, 0, &mut bump),
                    ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                        LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST,
                    ),
                ],
                Some(&mint_pubkey),
            ),
        )
        .unwrap();
    let data = bank_client.get_account_data(&pubkey).unwrap().unwrap();
    assert_eq!(0, data.len());

    // Realloc to 100 and check via CPI
    let invoke_account = AccountSharedData::new(START_BALANCE, 5, &realloc_invoke_program_id);
    bank.store_account(&invoke_pubkey, &invoke_account);
    bank_client
        .send_and_confirm_message(
            signer,
            Message::new(
                &[
                    Instruction::new_with_bytes(
                        realloc_invoke_program_id,
                        &[INVOKE_REALLOC_INVOKE_CHECK],
                        vec![
                            AccountMeta::new(invoke_pubkey, false),
                            AccountMeta::new_readonly(realloc_program_id, false),
                        ],
                    ),
                    ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                        LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST,
                    ),
                ],
                Some(&mint_pubkey),
            ),
        )
        .unwrap();
    let data = bank_client
        .get_account_data(&invoke_pubkey)
        .unwrap()
        .unwrap();
    assert_eq!(100, data.len());
    for i in 0..5 {
        assert_eq!(data[i], 0);
    }
    for i in 5..data.len() {
        assert_eq!(data[i], 2);
    }

    // Create account, realloc, check
    let new_keypair = Keypair::new();
    let new_pubkey = new_keypair.pubkey().clone();
    let mut instruction_data = vec![];
    instruction_data.extend_from_slice(&[INVOKE_CREATE_ACCOUNT_REALLOC_CHECK, 1]);
    instruction_data.extend_from_slice(&100_usize.to_le_bytes());
    bank_client
        .send_and_confirm_message(
            &[&mint_keypair, &new_keypair],
            Message::new(
                &[
                    Instruction::new_with_bytes(
                        realloc_invoke_program_id,
                        &instruction_data,
                        vec![
                            AccountMeta::new(mint_pubkey, true),
                            AccountMeta::new(new_pubkey, true),
                            AccountMeta::new(solana_system_interface::program::id(), false),
                            AccountMeta::new_readonly(realloc_invoke_program_id, false),
                        ],
                    ),
                    ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                        LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST,
                    ),
                ],
                Some(&mint_pubkey),
            ),
        )
        .unwrap();
    let data = bank_client.get_account_data(&new_pubkey).unwrap().unwrap();
    assert_eq!(200, data.len());
    let account = bank.get_account(&new_pubkey).unwrap();
    assert_eq!(&realloc_invoke_program_id, account.owner());

    // Invoke, dealloc, and assign
    let pre_len = 100;
    let new_len = pre_len * 2;
    let mut invoke_account = AccountSharedData::new(START_BALANCE, pre_len, &realloc_program_id);
    invoke_account.set_data_from_slice(&vec![1; pre_len]);
    bank.store_account(&invoke_pubkey, &invoke_account);
    let mut instruction_data = vec![];
    instruction_data.extend_from_slice(&[INVOKE_DEALLOC_AND_ASSIGN, 1]);
    instruction_data.extend_from_slice(&pre_len.to_le_bytes());
    bank_client
        .send_and_confirm_message(
            signer,
            Message::new(
                &[
                    Instruction::new_with_bytes(
                        realloc_invoke_program_id,
                        &instruction_data,
                        vec![
                            AccountMeta::new(invoke_pubkey, false),
                            AccountMeta::new_readonly(realloc_invoke_program_id, false),
                            AccountMeta::new_readonly(realloc_program_id, false),
                        ],
                    ),
                    ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                        LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST,
                    ),
                ],
                Some(&mint_pubkey),
            ),
        )
        .unwrap();
    let data = bank_client
        .get_account_data(&invoke_pubkey)
        .unwrap()
        .unwrap();
    assert_eq!(new_len, data.len());
    for i in 0..new_len {
        assert_eq!(data[i], 0);
    }

    // Realloc to max invoke max
    let invoke_account = AccountSharedData::new(42, 0, &realloc_invoke_program_id);
    bank.store_account(&invoke_pubkey, &invoke_account);
    assert_eq!(
        bank_client
            .send_and_confirm_message(
                signer,
                Message::new(
                    &[
                        Instruction::new_with_bytes(
                            realloc_invoke_program_id,
                            &[INVOKE_REALLOC_MAX_INVOKE_MAX],
                            vec![
                                AccountMeta::new(invoke_pubkey, false),
                                AccountMeta::new_readonly(realloc_program_id, false),
                            ],
                        ),
                        ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                            LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST
                        ),
                    ],
                    Some(&mint_pubkey),
                )
            )
            .unwrap_err()
            .unwrap(),
        TransactionError::InstructionError(0, InstructionError::InvalidRealloc)
    );

    // CPI realloc extend then local realloc extend
    for (cpi_extend_bytes, local_extend_bytes, should_succeed) in [
        (0, 0, true),
        (MAX_PERMITTED_DATA_INCREASE, 0, true),
        (0, MAX_PERMITTED_DATA_INCREASE, true),
        (MAX_PERMITTED_DATA_INCREASE, 1, false),
        (1, MAX_PERMITTED_DATA_INCREASE, false),
    ] {
        let invoke_account = AccountSharedData::new(100_000_000, 0, &realloc_invoke_program_id);
        bank.store_account(&invoke_pubkey, &invoke_account);
        let mut instruction_data = vec![];
        instruction_data.extend_from_slice(&[INVOKE_REALLOC_TO_THEN_LOCAL_REALLOC_EXTEND, 1]);
        instruction_data.extend_from_slice(&cpi_extend_bytes.to_le_bytes());
        instruction_data.extend_from_slice(&local_extend_bytes.to_le_bytes());

        let result = bank_client.send_and_confirm_message(
            signer,
            Message::new(
                &[
                    Instruction::new_with_bytes(
                        realloc_invoke_program_id,
                        &instruction_data,
                        vec![
                            AccountMeta::new(invoke_pubkey, false),
                            AccountMeta::new_readonly(realloc_invoke_program_id, false),
                        ],
                    ),
                    ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                        LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST,
                    ),
                ],
                Some(&mint_pubkey),
            ),
        );

        if should_succeed {
            assert!(
                result.is_ok(),
                "cpi: {cpi_extend_bytes} local: {local_extend_bytes}, err: {:?}",
                result.err()
            );
        } else {
            assert_eq!(
                result.unwrap_err().unwrap(),
                TransactionError::InstructionError(0, InstructionError::InvalidRealloc),
                "cpi: {cpi_extend_bytes} local: {local_extend_bytes}",
            );
        }
    }

    // Realloc invoke max twice
    let invoke_account = AccountSharedData::new(42, 0, &realloc_program_id);
    bank.store_account(&invoke_pubkey, &invoke_account);
    assert_eq!(
        bank_client
            .send_and_confirm_message(
                signer,
                Message::new(
                    &[
                        Instruction::new_with_bytes(
                            realloc_invoke_program_id,
                            &[INVOKE_INVOKE_MAX_TWICE],
                            vec![
                                AccountMeta::new(invoke_pubkey, false),
                                AccountMeta::new_readonly(realloc_invoke_program_id, false),
                                AccountMeta::new_readonly(realloc_program_id, false),
                            ],
                        ),
                        ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                            LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST
                        ),
                    ],
                    Some(&mint_pubkey),
                )
            )
            .unwrap_err()
            .unwrap(),
        TransactionError::InstructionError(0, InstructionError::InvalidRealloc)
    );

    // Realloc to 0
    bank_client
        .send_and_confirm_message(
            signer,
            Message::new(
                &[realloc(&realloc_program_id, &pubkey, 0, &mut bump)],
                Some(&mint_pubkey),
            ),
        )
        .unwrap();
    let data = bank_client.get_account_data(&pubkey).unwrap().unwrap();
    assert_eq!(0, data.len());

    // Realloc to max length in max increase increments
    for i in 0..MAX_PERMITTED_DATA_LENGTH as usize / MAX_PERMITTED_DATA_INCREASE {
        bank_client
            .send_and_confirm_message(
                signer,
                Message::new(
                    &[
                        Instruction::new_with_bytes(
                            realloc_invoke_program_id,
                            &[INVOKE_REALLOC_EXTEND_MAX, 1, i as u8, (i / 255) as u8],
                            vec![
                                AccountMeta::new(pubkey, false),
                                AccountMeta::new_readonly(realloc_program_id, false),
                            ],
                        ),
                        ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                            LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST,
                        ),
                    ],
                    Some(&mint_pubkey),
                ),
            )
            .unwrap();
        let data = bank_client.get_account_data(&pubkey).unwrap().unwrap();
        assert_eq!((i + 1) * MAX_PERMITTED_DATA_INCREASE, data.len());
    }
    for i in 0..data.len() {
        assert_eq!(data[i], 1);
    }

    // and one more time should fail
    assert_eq!(
        bank_client
            .send_and_confirm_message(
                signer,
                Message::new(
                    &[
                        Instruction::new_with_bytes(
                            realloc_invoke_program_id,
                            &[INVOKE_REALLOC_EXTEND_MAX, 2, 1, 1],
                            vec![
                                AccountMeta::new(pubkey, false),
                                AccountMeta::new_readonly(realloc_program_id, false),
                            ],
                        ),
                        ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                            LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST
                        ),
                    ],
                    Some(&mint_pubkey),
                )
            )
            .unwrap_err()
            .unwrap(),
        TransactionError::InstructionError(0, InstructionError::InvalidRealloc)
    );

    // Realloc recursively and fill data
    let invoke_keypair = Keypair::new();
    let invoke_pubkey = invoke_keypair.pubkey().clone();
    let invoke_account = AccountSharedData::new(START_BALANCE, 0, &realloc_invoke_program_id);
    bank.store_account(&invoke_pubkey, &invoke_account);
    let mut instruction_data = vec![];
    instruction_data.extend_from_slice(&[INVOKE_REALLOC_RECURSIVE, 1]);
    instruction_data.extend_from_slice(&100_usize.to_le_bytes());
    bank_client
        .send_and_confirm_message(
            signer,
            Message::new(
                &[
                    Instruction::new_with_bytes(
                        realloc_invoke_program_id,
                        &instruction_data,
                        vec![
                            AccountMeta::new(invoke_pubkey, false),
                            AccountMeta::new_readonly(realloc_invoke_program_id, false),
                        ],
                    ),
                    ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                        LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST,
                    ),
                ],
                Some(&mint_pubkey),
            ),
        )
        .unwrap();
    let data = bank_client
        .get_account_data(&invoke_pubkey)
        .unwrap()
        .unwrap();
    assert_eq!(200, data.len());
    for i in 0..100 {
        assert_eq!(data[i], 1);
    }
    for i in 100..200 {
        assert_eq!(data[i], 2);
    }
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_processed_inner_instruction() {
    solana_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50);

    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let mut bank_client = BankClient::new_shared(bank.clone());
    let authority_keypair = Keypair::new();

    let (_bank, sibling_program_id) = load_program_of_loader_v4(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_sibling_instructions",
    );
    let (_bank, sibling_inner_program_id) = load_program_of_loader_v4(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_sibling_inner_instructions",
    );
    let (_bank, noop_program_id) = load_program_of_loader_v4(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_noop",
    );
    let (_bank, invoke_and_return_program_id) = load_program_of_loader_v4(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_invoke_and_return",
    );

    let instruction2 = Instruction::new_with_bytes(
        noop_program_id,
        &[43],
        vec![
            AccountMeta::new_readonly(noop_program_id, false),
            AccountMeta::new(mint_keypair.pubkey(), true),
        ],
    );
    let instruction1 = Instruction::new_with_bytes(
        noop_program_id,
        &[42],
        vec![
            AccountMeta::new(mint_keypair.pubkey(), true),
            AccountMeta::new_readonly(noop_program_id, false),
        ],
    );
    let instruction0 = Instruction::new_with_bytes(
        sibling_program_id,
        &[1, 2, 3, 0, 4, 5, 6],
        vec![
            AccountMeta::new(mint_keypair.pubkey(), true),
            AccountMeta::new_readonly(noop_program_id, false),
            AccountMeta::new_readonly(invoke_and_return_program_id, false),
            AccountMeta::new_readonly(sibling_inner_program_id, false),
        ],
    );
    let message = Message::new(
        &[instruction2, instruction1, instruction0],
        Some(&mint_keypair.pubkey()),
    );
    assert!(bank_client
        .send_and_confirm_message(&[&mint_keypair], message)
        .is_ok());
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_fees() {
    solana_logger::setup();

    let congestion_multiplier = 1;

    let GenesisConfigInfo {
        mut genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(500_000_000);

    genesis_config.fee_rate_governor = FeeRateGovernor::new(congestion_multiplier, 0);
    let mut bank = Bank::new_for_tests(&genesis_config);
    let fee_structure = FeeStructure {
        lamports_per_signature: 5000,
        lamports_per_write_lock: 0,
        compute_fee_bins: vec![
            FeeBin {
                limit: 200,
                fee: 500,
            },
            FeeBin {
                limit: 1400000,
                fee: 5000,
            },
        ],
    };
    bank.set_fee_structure(&fee_structure);
    let (bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();
    let feature_set = bank.feature_set.clone();
    let mut bank_client = BankClient::new_shared(bank.clone());
    let authority_keypair = Keypair::new();

    let (_bank, program_id) = load_program_of_loader_v4(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_noop",
    );

    let pre_balance = bank_client.get_balance(&mint_keypair.pubkey()).unwrap();
    let message = Message::new(
        &[Instruction::new_with_bytes(program_id, &[], vec![])],
        Some(&mint_keypair.pubkey()),
    );

    let sanitized_message = SanitizedMessage::try_from_legacy_message(
        message.clone(),
        &ReservedAccountKeys::empty_key_set(),
    )
    .unwrap();
    let fee_budget_limits = FeeBudgetLimits::from(
        process_compute_budget_instructions(
            SVMMessage::program_instructions_iter(&sanitized_message),
            &feature_set,
        )
        .unwrap_or_default(),
    );
    let expected_normal_fee = solana_fee::calculate_fee(
        &sanitized_message,
        congestion_multiplier == 0,
        fee_structure.lamports_per_signature,
        fee_budget_limits.prioritization_fee,
        bank.feature_set.as_ref().into(),
    );
    bank_client
        .send_and_confirm_message(&[&mint_keypair], message)
        .unwrap();
    let post_balance = bank_client.get_balance(&mint_keypair.pubkey()).unwrap();
    assert_eq!(pre_balance - post_balance, expected_normal_fee);

    let pre_balance = bank_client.get_balance(&mint_keypair.pubkey()).unwrap();
    let message = Message::new(
        &[
            ComputeBudgetInstruction::set_compute_unit_price(1),
            Instruction::new_with_bytes(program_id, &[], vec![]),
        ],
        Some(&mint_keypair.pubkey()),
    );
    let sanitized_message = SanitizedMessage::try_from_legacy_message(
        message.clone(),
        &ReservedAccountKeys::empty_key_set(),
    )
    .unwrap();
    let fee_budget_limits = FeeBudgetLimits::from(
        process_compute_budget_instructions(
            SVMMessage::program_instructions_iter(&sanitized_message),
            &feature_set,
        )
        .unwrap_or_default(),
    );
    let expected_prioritized_fee = solana_fee::calculate_fee(
        &sanitized_message,
        congestion_multiplier == 0,
        fee_structure.lamports_per_signature,
        fee_budget_limits.prioritization_fee,
        bank.feature_set.as_ref().into(),
    );
    assert!(expected_normal_fee < expected_prioritized_fee);

    bank_client
        .send_and_confirm_message(&[&mint_keypair], message)
        .unwrap();
    let post_balance = bank_client.get_balance(&mint_keypair.pubkey()).unwrap();
    assert_eq!(pre_balance - post_balance, expected_prioritized_fee);
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_get_minimum_delegation() {
    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(100_123_456_789);

    let bank = Bank::new_for_tests(&genesis_config);
    let (bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();
    let mut bank_client = BankClient::new_shared(bank.clone());
    let authority_keypair = Keypair::new();

    let (_bank, program_id) = load_program_of_loader_v4(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_get_minimum_delegation",
    );

    let account_metas = vec![AccountMeta::new_readonly(stake::program::id(), false)];
    let instruction = Instruction::new_with_bytes(program_id, &[], account_metas);
    let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
    assert!(result.is_ok());
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_inner_instruction_alignment_checks() {
    solana_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50);
    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let noop = create_program(&bank, &bpf_loader_deprecated::id(), "solana_sbf_rust_noop");
    let inner_instruction_alignment_check = create_program(
        &bank,
        &bpf_loader_deprecated::id(),
        "solana_sbf_rust_inner_instruction_alignment_check",
    );

    // invoke unaligned program, which will call aligned program twice,
    // unaligned should be allowed once invoke completes
    let mut bank_client = BankClient::new_shared(bank);
    bank_client
        .advance_slot(1, bank_forks.as_ref(), &Pubkey::default())
        .expect("Failed to advance the slot");
    let mut instruction = Instruction::new_with_bytes(
        inner_instruction_alignment_check,
        &[0],
        vec![
            AccountMeta::new_readonly(noop, false),
            AccountMeta::new_readonly(mint_keypair.pubkey(), false),
        ],
    );

    instruction.data[0] += 1;
    let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction.clone());
    assert!(result.is_ok(), "{result:?}");
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_cpi_account_ownership_writability() {
    solana_logger::setup();

    for direct_mapping in [false, true] {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(100_123_456_789);

        let mut bank = Bank::new_for_tests(&genesis_config);
        let mut feature_set = FeatureSet::all_enabled();
        if !direct_mapping {
            feature_set.deactivate(&feature_set::bpf_account_data_direct_mapping::id());
        }

        bank.feature_set = Arc::new(feature_set);
        let (bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();
        let mut bank_client = BankClient::new_shared(bank);
        let authority_keypair = Keypair::new();

        let (_bank, invoke_program_id) = load_program_of_loader_v4(
            &mut bank_client,
            &bank_forks,
            &mint_keypair,
            &authority_keypair,
            "solana_sbf_rust_invoke",
        );
        let (_bank, invoked_program_id) = load_program_of_loader_v4(
            &mut bank_client,
            &bank_forks,
            &mint_keypair,
            &authority_keypair,
            "solana_sbf_rust_invoked",
        );
        let (bank, realloc_program_id) = load_program_of_loader_v4(
            &mut bank_client,
            &bank_forks,
            &mint_keypair,
            &authority_keypair,
            "solana_sbf_rust_realloc",
        );

        let account_keypair = Keypair::new();

        let mint_pubkey = mint_keypair.pubkey();
        let account_metas = vec![
            AccountMeta::new(mint_pubkey, true),
            AccountMeta::new(account_keypair.pubkey(), false),
            AccountMeta::new_readonly(invoked_program_id, false),
            AccountMeta::new_readonly(invoke_program_id, false),
            AccountMeta::new_readonly(realloc_program_id, false),
        ];

        for (account_size, byte_index) in [
            (0, 0),                                   // first realloc byte
            (0, MAX_PERMITTED_DATA_INCREASE - 1),     // last realloc byte
            (2, 0),                                   // first data byte
            (2, 1),                                   // last data byte
            (2, 3),                                   // first realloc byte
            (2, 2 + MAX_PERMITTED_DATA_INCREASE - 1), // last realloc byte
        ] {
            for instruction_id in [
                TEST_FORBID_WRITE_AFTER_OWNERSHIP_CHANGE_IN_CALLEE,
                TEST_FORBID_WRITE_AFTER_OWNERSHIP_CHANGE_IN_CALLER,
            ] {
                bank.register_unique_recent_blockhash_for_test();
                let account = AccountSharedData::new(42, account_size, &invoke_program_id);
                bank.store_account(&account_keypair.pubkey(), &account);
                let mut instruction_data = vec![instruction_id];
                instruction_data.extend_from_slice(byte_index.to_le_bytes().as_ref());

                let instruction = Instruction::new_with_bytes(
                    invoke_program_id,
                    &instruction_data,
                    account_metas.clone(),
                );

                let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);

                if (byte_index as usize) < account_size || direct_mapping {
                    assert_eq!(
                        result.unwrap_err().unwrap(),
                        TransactionError::InstructionError(
                            0,
                            InstructionError::ExternalAccountDataModified,
                        )
                    );
                } else {
                    // without direct mapping, changes to the realloc padding
                    // outside the account length are ignored
                    assert!(result.is_ok(), "{result:?}");
                }
            }
        }
        // Test that the CPI code that updates `ref_to_len_in_vm` fails if we
        // make it write to an invalid location. This is the first variant which
        // correctly triggers ExternalAccountDataModified when direct mapping is
        // disabled. When direct mapping is enabled this tests fails early
        // because we move the account data pointer.
        // TEST_FORBID_LEN_UPDATE_AFTER_OWNERSHIP_CHANGE is able to make more
        // progress when direct mapping is on.
        let account = AccountSharedData::new(42, 0, &invoke_program_id);
        bank.store_account(&account_keypair.pubkey(), &account);
        let instruction_data = vec![
            TEST_FORBID_LEN_UPDATE_AFTER_OWNERSHIP_CHANGE_MOVING_DATA_POINTER,
            42,
            42,
            42,
        ];
        let instruction = Instruction::new_with_bytes(
            invoke_program_id,
            &instruction_data,
            account_metas.clone(),
        );
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        assert_eq!(
            result.unwrap_err().unwrap(),
            if direct_mapping {
                // We move the data pointer, direct mapping doesn't allow it
                // anymore so it errors out earlier. See
                // test_cpi_invalid_account_info_pointers.
                TransactionError::InstructionError(0, InstructionError::ProgramFailedToComplete)
            } else {
                // We managed to make CPI write into the account data, but the
                // usual checks still apply and we get an error.
                TransactionError::InstructionError(0, InstructionError::ExternalAccountDataModified)
            }
        );

        // We're going to try and make CPI write ref_to_len_in_vm into a 2nd
        // account, so we add an extra one here.
        let account2_keypair = Keypair::new();
        let mut account_metas = account_metas.clone();
        account_metas.push(AccountMeta::new(account2_keypair.pubkey(), false));

        for target_account in [1, account_metas.len() as u8 - 1] {
            // Similar to the test above where we try to make CPI write into account
            // data. This variant is for when direct mapping is enabled.
            let account = AccountSharedData::new(42, 0, &invoke_program_id);
            bank.store_account(&account_keypair.pubkey(), &account);
            let account = AccountSharedData::new(42, 0, &invoke_program_id);
            bank.store_account(&account2_keypair.pubkey(), &account);
            let instruction_data = vec![
                TEST_FORBID_LEN_UPDATE_AFTER_OWNERSHIP_CHANGE,
                target_account,
                42,
                42,
            ];
            let instruction = Instruction::new_with_bytes(
                invoke_program_id,
                &instruction_data,
                account_metas.clone(),
            );
            let message = Message::new(&[instruction], Some(&mint_pubkey));
            let tx = Transaction::new(&[&mint_keypair], message.clone(), bank.last_blockhash());
            let (result, _, logs, _) = process_transaction_and_record_inner(&bank, tx);
            if direct_mapping {
                assert_eq!(
                    result.unwrap_err(),
                    TransactionError::InstructionError(
                        0,
                        InstructionError::ProgramFailedToComplete
                    )
                );
                // We haven't moved the data pointer, but ref_to_len_vm _is_ in
                // the account data vm range and that's not allowed either.
                assert!(
                    logs.iter().any(|log| log.contains("Invalid pointer")),
                    "{logs:?}"
                );
            } else {
                // we expect this to succeed as after updating `ref_to_len_in_vm`,
                // CPI will sync the actual account data between the callee and the
                // caller, _always_ writing over the location pointed by
                // `ref_to_len_in_vm`. To verify this, we check that the account
                // data is in fact all zeroes like it is in the callee.
                result.unwrap();
                let account = bank.get_account(&account_keypair.pubkey()).unwrap();
                assert_eq!(account.data(), vec![0; 40]);
            }
        }

        // Test that the caller can write to an account which it received from the callee
        let account = AccountSharedData::new(42, 0, &invoked_program_id);
        bank.store_account(&account_keypair.pubkey(), &account);
        let instruction_data = vec![TEST_ALLOW_WRITE_AFTER_OWNERSHIP_CHANGE_TO_CALLER, 1, 42, 42];
        let instruction =
            Instruction::new_with_bytes(invoke_program_id, &instruction_data, account_metas);
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        result.unwrap();
    }
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_cpi_account_data_updates() {
    solana_logger::setup();

    for (deprecated_callee, deprecated_caller, direct_mapping) in
        [false, true].into_iter().flat_map(move |z| {
            [false, true]
                .into_iter()
                .flat_map(move |y| [false, true].into_iter().map(move |x| (x, y, z)))
        })
    {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(100_123_456_789);
        let mut bank = Bank::new_for_tests(&genesis_config);
        let mut feature_set = FeatureSet::all_enabled();
        if !direct_mapping {
            feature_set.deactivate(&feature_set::bpf_account_data_direct_mapping::id());
        }

        bank.feature_set = Arc::new(feature_set);
        let (bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();
        let mut bank_client = BankClient::new_shared(bank);
        let authority_keypair = Keypair::new();

        let (_bank, invoke_program_id) = load_program_of_loader_v4(
            &mut bank_client,
            &bank_forks,
            &mint_keypair,
            &authority_keypair,
            "solana_sbf_rust_invoke",
        );
        let (bank, realloc_program_id) = load_program_of_loader_v4(
            &mut bank_client,
            &bank_forks,
            &mint_keypair,
            &authority_keypair,
            "solana_sbf_rust_realloc",
        );
        let deprecated_program_id = create_program(
            &bank,
            &bpf_loader_deprecated::id(),
            "solana_sbf_rust_deprecated_loader",
        );

        let account_keypair = Keypair::new();
        let mint_pubkey = mint_keypair.pubkey();
        let account_metas = vec![
            AccountMeta::new(mint_pubkey, true),
            AccountMeta::new(account_keypair.pubkey(), false),
            AccountMeta::new_readonly(
                if deprecated_callee {
                    deprecated_program_id
                } else {
                    realloc_program_id
                },
                false,
            ),
            AccountMeta::new_readonly(
                if deprecated_caller {
                    deprecated_program_id
                } else {
                    invoke_program_id
                },
                false,
            ),
        ];

        // This tests the case where a caller extends an account beyond the original
        // data length. The callee should see the extended data (asserted in the
        // callee program, not here).
        let mut account = AccountSharedData::new(42, 0, &account_metas[3].pubkey);
        account.set_data(b"foo".to_vec());
        bank.store_account(&account_keypair.pubkey(), &account);
        let mut instruction_data = vec![TEST_CPI_ACCOUNT_UPDATE_CALLER_GROWS];
        instruction_data.extend_from_slice(b"bar");
        let instruction = Instruction::new_with_bytes(
            account_metas[3].pubkey,
            &instruction_data,
            account_metas.clone(),
        );
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        if deprecated_caller {
            assert_eq!(
                result.unwrap_err().unwrap(),
                TransactionError::InstructionError(
                    0,
                    if direct_mapping {
                        InstructionError::ProgramFailedToComplete
                    } else {
                        InstructionError::ModifiedProgramId
                    }
                )
            );
        } else {
            assert!(result.is_ok(), "{result:?}");
            let account = bank.get_account(&account_keypair.pubkey()).unwrap();
            // "bar" here was copied from the realloc region
            assert_eq!(account.data(), b"foobar");
        }

        // This tests the case where a callee extends an account beyond the original
        // data length. The caller should see the extended data where the realloc
        // region contains the new data. In this test the callee owns the account,
        // the caller can't write but the CPI glue still updates correctly.
        let mut account = AccountSharedData::new(42, 0, &account_metas[2].pubkey);
        account.set_data(b"foo".to_vec());
        bank.store_account(&account_keypair.pubkey(), &account);
        let mut instruction_data = vec![TEST_CPI_ACCOUNT_UPDATE_CALLEE_GROWS];
        instruction_data.extend_from_slice(b"bar");
        let instruction = Instruction::new_with_bytes(
            account_metas[3].pubkey,
            &instruction_data,
            account_metas.clone(),
        );
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        if deprecated_callee {
            assert!(result.is_ok(), "{result:?}");
            let account = bank.get_account(&account_keypair.pubkey()).unwrap();
            // deprecated_callee is incapable of resizing accounts
            assert_eq!(account.data(), b"foo");
        } else if deprecated_caller {
            assert_eq!(
                result.unwrap_err().unwrap(),
                TransactionError::InstructionError(
                    0,
                    if direct_mapping {
                        InstructionError::InvalidRealloc
                    } else {
                        InstructionError::AccountDataSizeChanged
                    }
                )
            );
        } else {
            assert!(result.is_ok(), "{result:?}");
            let account = bank.get_account(&account_keypair.pubkey()).unwrap();
            // "bar" here was copied from the realloc region
            assert_eq!(account.data(), b"foobar");
        }

        // This tests the case where a callee shrinks an account, the caller data
        // slice must be truncated accordingly and post_len..original_data_len must
        // be zeroed (zeroing is checked in the invoked program not here). Same as
        // above, the callee owns the account but the changes are still reflected in
        // the caller even if things are readonly from the caller's POV.
        let mut account = AccountSharedData::new(42, 0, &account_metas[2].pubkey);
        account.set_data(b"foobar".to_vec());
        bank.store_account(&account_keypair.pubkey(), &account);
        let mut instruction_data = vec![
            TEST_CPI_ACCOUNT_UPDATE_CALLEE_SHRINKS_SMALLER_THAN_ORIGINAL_LEN,
            direct_mapping as u8,
        ];
        instruction_data.extend_from_slice(4usize.to_le_bytes().as_ref());
        let instruction = Instruction::new_with_bytes(
            account_metas[3].pubkey,
            &instruction_data,
            account_metas.clone(),
        );
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        if deprecated_callee {
            assert!(result.is_ok(), "{result:?}");
            let account = bank.get_account(&account_keypair.pubkey()).unwrap();
            // deprecated_callee is incapable of resizing accounts
            assert_eq!(account.data(), b"foobar");
        } else if deprecated_caller {
            assert_eq!(
                result.unwrap_err().unwrap(),
                TransactionError::InstructionError(
                    0,
                    if direct_mapping && deprecated_callee {
                        InstructionError::InvalidRealloc
                    } else {
                        InstructionError::AccountDataSizeChanged
                    }
                )
            );
        } else {
            assert!(result.is_ok(), "{result:?}");
            let account = bank.get_account(&account_keypair.pubkey()).unwrap();
            assert_eq!(account.data(), b"foob");
        }

        // This tests the case where the program extends an account, then calls
        // itself and in the inner call it shrinks the account to a size that is
        // still larger than the original size. The account data must be set to the
        // correct value in the caller frame, and the realloc region must be zeroed
        // (again tested in the invoked program).
        let mut account = AccountSharedData::new(42, 0, &account_metas[3].pubkey);
        account.set_data(b"foo".to_vec());
        bank.store_account(&account_keypair.pubkey(), &account);
        let mut instruction_data = vec![
            TEST_CPI_ACCOUNT_UPDATE_CALLER_GROWS_CALLEE_SHRINKS,
            direct_mapping as u8,
        ];
        // realloc to "foobazbad" then shrink to "foobazb"
        instruction_data.extend_from_slice(7usize.to_le_bytes().as_ref());
        instruction_data.extend_from_slice(b"bazbad");
        let instruction = Instruction::new_with_bytes(
            account_metas[3].pubkey,
            &instruction_data,
            account_metas.clone(),
        );
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        if deprecated_caller {
            assert_eq!(
                result.unwrap_err().unwrap(),
                TransactionError::InstructionError(
                    0,
                    if direct_mapping {
                        InstructionError::ProgramFailedToComplete
                    } else {
                        InstructionError::ModifiedProgramId
                    }
                )
            );
        } else {
            assert!(result.is_ok(), "{result:?}");
            let account = bank.get_account(&account_keypair.pubkey()).unwrap();
            assert_eq!(account.data(), b"foobazb");
        }

        // Similar to the test above, but this time the nested invocation shrinks to
        // _below_ the original data length. Both the spare capacity in the account
        // data _end_ the realloc region must be zeroed.
        let mut account = AccountSharedData::new(42, 0, &account_metas[3].pubkey);
        account.set_data(b"foo".to_vec());
        bank.store_account(&account_keypair.pubkey(), &account);
        let mut instruction_data = vec![
            TEST_CPI_ACCOUNT_UPDATE_CALLER_GROWS_CALLEE_SHRINKS,
            direct_mapping as u8,
        ];
        // realloc to "foobazbad" then shrink to "f"
        instruction_data.extend_from_slice(1usize.to_le_bytes().as_ref());
        instruction_data.extend_from_slice(b"bazbad");
        let instruction = Instruction::new_with_bytes(
            account_metas[3].pubkey,
            &instruction_data,
            account_metas.clone(),
        );
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        if deprecated_caller {
            assert_eq!(
                result.unwrap_err().unwrap(),
                TransactionError::InstructionError(
                    0,
                    if direct_mapping {
                        InstructionError::ProgramFailedToComplete
                    } else {
                        InstructionError::ModifiedProgramId
                    }
                )
            );
        } else {
            assert!(result.is_ok(), "{result:?}");
            let account = bank.get_account(&account_keypair.pubkey()).unwrap();
            assert_eq!(account.data(), b"f");
        }
    }
}

#[test]
#[cfg(any(feature = "sbf_c", feature = "sbf_rust"))]
fn test_cpi_invalid_account_info_pointers() {
    solana_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(100_123_456_789);

    let bank = Bank::new_for_tests(&genesis_config);
    let (bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();
    let mut bank_client = BankClient::new_shared(bank);
    let authority_keypair = Keypair::new();

    let account_keypair = Keypair::new();
    let mint_pubkey = mint_keypair.pubkey();
    let mut account_metas = vec![
        AccountMeta::new(mint_pubkey, true),
        AccountMeta::new(account_keypair.pubkey(), false),
    ];

    let mut program_ids: Vec<Pubkey> = Vec::with_capacity(2);

    #[allow(unused_mut)]
    let mut bank;
    #[cfg(feature = "sbf_rust")]
    {
        let (new_bank, invoke_program_id) = load_program_of_loader_v4(
            &mut bank_client,
            &bank_forks,
            &mint_keypair,
            &authority_keypair,
            "solana_sbf_rust_invoke",
        );
        account_metas.push(AccountMeta::new_readonly(invoke_program_id, false));
        program_ids.push(invoke_program_id);
        #[allow(unused)]
        {
            bank = new_bank;
        }
    }

    #[cfg(feature = "sbf_c")]
    {
        let (new_bank, c_invoke_program_id) = load_program_of_loader_v4(
            &mut bank_client,
            &bank_forks,
            &mint_keypair,
            &authority_keypair,
            "invoke",
        );
        account_metas.push(AccountMeta::new_readonly(c_invoke_program_id, false));
        program_ids.push(c_invoke_program_id);
        #[allow(unused)]
        {
            bank = new_bank;
        }
    }

    for invoke_program_id in &program_ids {
        for ix in [
            TEST_CPI_INVALID_KEY_POINTER,
            TEST_CPI_INVALID_LAMPORTS_POINTER,
            TEST_CPI_INVALID_OWNER_POINTER,
            TEST_CPI_INVALID_DATA_POINTER,
        ] {
            let account = AccountSharedData::new(42, 5, invoke_program_id);
            bank.store_account(&account_keypair.pubkey(), &account);
            let instruction = Instruction::new_with_bytes(
                *invoke_program_id,
                &[ix, 42, 42, 42],
                account_metas.clone(),
            );

            let message = Message::new(&[instruction], Some(&mint_pubkey));
            let tx = Transaction::new(&[&mint_keypair], message.clone(), bank.last_blockhash());
            let (result, _, logs, _) = process_transaction_and_record_inner(&bank, tx);
            assert!(result.is_err(), "{result:?}");
            assert!(
                logs.iter().any(|log| log.contains("Invalid pointer")),
                "{logs:?}"
            );
        }
    }
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_deplete_cost_meter_with_access_violation() {
    solana_logger::setup();
    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(100_123_456_789);

    let bank = Bank::new_for_tests(&genesis_config);
    let (bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();
    let mut bank_client = BankClient::new_shared(bank.clone());
    let authority_keypair = Keypair::new();
    let (bank, invoke_program_id) = load_program_of_loader_v4(
        &mut bank_client,
        bank_forks.as_ref(),
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_invoke",
    );

    let account_keypair = Keypair::new();
    let mint_pubkey = mint_keypair.pubkey();
    let account_metas = vec![
        AccountMeta::new(mint_pubkey, true),
        AccountMeta::new(account_keypair.pubkey(), false),
        AccountMeta::new_readonly(invoke_program_id, false),
    ];

    let mut instruction_data = vec![TEST_WRITE_ACCOUNT, 2];
    instruction_data.extend_from_slice(3usize.to_le_bytes().as_ref());
    instruction_data.push(42);

    let instruction =
        Instruction::new_with_bytes(invoke_program_id, &instruction_data, account_metas.clone());

    let compute_unit_limit = 10_000u32;
    let message = Message::new(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(compute_unit_limit),
            instruction,
        ],
        Some(&mint_keypair.pubkey()),
    );
    let tx = Transaction::new(&[&mint_keypair], message, bank.last_blockhash());

    let result = load_execute_and_commit_transaction(&bank, tx).unwrap();

    assert_eq!(
        result.status.unwrap_err(),
        TransactionError::InstructionError(1, InstructionError::ReadonlyDataModified)
    );

    // all compute unit limit should be consumed due to SBF VM error
    assert_eq!(result.executed_units, u64::from(compute_unit_limit));
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_deplete_cost_meter_with_divide_by_zero() {
    solana_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50);

    let bank = Bank::new_for_tests(&genesis_config);
    let (bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();
    let mut bank_client = BankClient::new_shared(bank.clone());
    let authority_keypair = Keypair::new();
    let (bank, program_id) = load_program_of_loader_v4(
        &mut bank_client,
        bank_forks.as_ref(),
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_divide_by_zero",
    );

    let instruction = Instruction::new_with_bytes(program_id, &[], vec![]);
    let compute_unit_limit = 10_000;
    let message = Message::new(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(compute_unit_limit),
            instruction,
        ],
        Some(&mint_keypair.pubkey()),
    );
    let tx = Transaction::new(&[&mint_keypair], message, bank.last_blockhash());

    let result = load_execute_and_commit_transaction(&bank, tx).unwrap();

    assert_eq!(
        result.status.unwrap_err(),
        TransactionError::InstructionError(1, InstructionError::ProgramFailedToComplete)
    );

    // all compute unit limit should be consumed due to SBF VM error
    assert_eq!(result.executed_units, u64::from(compute_unit_limit));
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_deny_access_beyond_current_length() {
    solana_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(100_123_456_789);

    for direct_mapping in [false, true] {
        let mut bank = Bank::new_for_tests(&genesis_config);
        let feature_set = Arc::make_mut(&mut bank.feature_set);
        // by default test banks have all features enabled, so we only need to
        // disable when needed
        if !direct_mapping {
            feature_set.deactivate(&feature_set::bpf_account_data_direct_mapping::id());
        }
        let (bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();
        let mut bank_client = BankClient::new_shared(bank);
        let authority_keypair = Keypair::new();

        let (bank, invoke_program_id) = load_program_of_loader_v4(
            &mut bank_client,
            &bank_forks,
            &mint_keypair,
            &authority_keypair,
            "solana_sbf_rust_invoke",
        );
        let account = AccountSharedData::new(42, 0, &invoke_program_id);
        let readonly_account_keypair = Keypair::new();
        let writable_account_keypair = Keypair::new();
        bank.store_account(&readonly_account_keypair.pubkey(), &account);
        bank.store_account(&writable_account_keypair.pubkey(), &account);

        let mint_pubkey = mint_keypair.pubkey();
        let account_metas = vec![
            AccountMeta::new(mint_pubkey, true),
            AccountMeta::new_readonly(readonly_account_keypair.pubkey(), false),
            AccountMeta::new(writable_account_keypair.pubkey(), false),
            AccountMeta::new_readonly(invoke_program_id, false),
        ];

        for (instruction_account_index, expected_error) in [
            (1, InstructionError::AccountDataTooSmall),
            (2, InstructionError::InvalidRealloc),
        ] {
            let mut instruction_data = vec![TEST_READ_ACCOUNT, instruction_account_index];
            instruction_data.extend_from_slice(3usize.to_le_bytes().as_ref());
            let instruction = Instruction::new_with_bytes(
                invoke_program_id,
                &instruction_data,
                account_metas.clone(),
            );
            let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
            if direct_mapping {
                assert_eq!(
                    result.unwrap_err().unwrap(),
                    TransactionError::InstructionError(0, expected_error)
                );
            } else {
                result.unwrap();
            }
        }
    }
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_deny_executable_write() {
    solana_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(100_123_456_789);

    for direct_mapping in [false, true] {
        let mut bank = Bank::new_for_tests(&genesis_config);
        let feature_set = Arc::make_mut(&mut bank.feature_set);
        // by default test banks have all features enabled, so we only need to
        // disable when needed
        if !direct_mapping {
            feature_set.deactivate(&feature_set::bpf_account_data_direct_mapping::id());
        }
        let (bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();
        let mut bank_client = BankClient::new_shared(bank);
        let authority_keypair = Keypair::new();

        let (_bank, invoke_program_id) = load_program_of_loader_v4(
            &mut bank_client,
            &bank_forks,
            &mint_keypair,
            &authority_keypair,
            "solana_sbf_rust_invoke",
        );

        let account_keypair = Keypair::new();
        let mint_pubkey = mint_keypair.pubkey();
        let account_metas = vec![
            AccountMeta::new(mint_pubkey, true),
            AccountMeta::new(account_keypair.pubkey(), false),
            AccountMeta::new_readonly(invoke_program_id, false),
        ];

        let mut instruction_data = vec![TEST_WRITE_ACCOUNT, 2];
        instruction_data.extend_from_slice(3usize.to_le_bytes().as_ref());
        instruction_data.push(42);
        let instruction = Instruction::new_with_bytes(
            invoke_program_id,
            &instruction_data,
            account_metas.clone(),
        );
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        assert_eq!(
            result.unwrap_err().unwrap(),
            TransactionError::InstructionError(0, InstructionError::ReadonlyDataModified)
        );
    }
}

#[test]
fn test_update_callee_account() {
    // Test that fn update_callee_account() works and we are updating the callee account on CPI.
    solana_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(100_123_456_789);

    for direct_mapping in [false, true] {
        let mut bank = Bank::new_for_tests(&genesis_config);
        let feature_set = Arc::make_mut(&mut bank.feature_set);
        // by default test banks have all features enabled, so we only need to
        // disable when needed
        if !direct_mapping {
            feature_set.deactivate(&feature_set::bpf_account_data_direct_mapping::id());
        }
        let (bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();
        let mut bank_client = BankClient::new_shared(bank.clone());
        let authority_keypair = Keypair::new();

        let (bank, invoke_program_id) = load_program_of_loader_v4(
            &mut bank_client,
            &bank_forks,
            &mint_keypair,
            &authority_keypair,
            "solana_sbf_rust_invoke",
        );

        let account_keypair = Keypair::new();

        let mint_pubkey = mint_keypair.pubkey();

        let account_metas = vec![
            AccountMeta::new(mint_pubkey, true),
            AccountMeta::new(account_keypair.pubkey(), false),
            AccountMeta::new_readonly(invoke_program_id, false),
        ];

        // I. do CPI with account in read only (separate code path with direct mapping)
        let mut account = AccountSharedData::new(42, 10240, &invoke_program_id);
        let data: Vec<u8> = (0..10240).map(|n| n as u8).collect();
        account.set_data(data);

        bank.store_account(&account_keypair.pubkey(), &account);

        let mut instruction_data = vec![TEST_CALLEE_ACCOUNT_UPDATES, 0, 0];
        instruction_data.extend_from_slice(20480usize.to_le_bytes().as_ref());
        instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
        instruction_data.extend_from_slice(16384usize.to_le_bytes().as_ref());
        // instruction data for inner CPI (2x)
        instruction_data.extend_from_slice(&[TEST_CALLEE_ACCOUNT_UPDATES, 0, 0]);
        instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
        instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
        instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());

        instruction_data.extend_from_slice(&[TEST_CALLEE_ACCOUNT_UPDATES, 0, 0]);
        instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
        instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
        instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());

        let instruction = Instruction::new_with_bytes(
            invoke_program_id,
            &instruction_data,
            account_metas.clone(),
        );
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        assert!(result.is_ok());

        let data = bank_client
            .get_account_data(&account_keypair.pubkey())
            .unwrap()
            .unwrap();

        assert_eq!(data.len(), 20480);

        data.iter().enumerate().for_each(|(i, v)| {
            let expected = match i {
                ..=10240 => i as u8,
                16384 => 0xe5,
                _ => 0,
            };

            assert_eq!(*v, expected, "offset:{i} {v:#x} != {expected:#x}");
        });

        // II. do CPI with account with resize to smaller and write
        let mut account = AccountSharedData::new(42, 10240, &invoke_program_id);
        let data: Vec<u8> = (0..10240).map(|n| n as u8).collect();
        account.set_data(data);
        bank.store_account(&account_keypair.pubkey(), &account);

        let mut instruction_data = vec![TEST_CALLEE_ACCOUNT_UPDATES, 1, 0];
        instruction_data.extend_from_slice(20480usize.to_le_bytes().as_ref());
        instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
        instruction_data.extend_from_slice(16384usize.to_le_bytes().as_ref());
        // instruction data for inner CPI
        instruction_data.extend_from_slice(&[TEST_CALLEE_ACCOUNT_UPDATES, 0, 0]);
        instruction_data.extend_from_slice(19480usize.to_le_bytes().as_ref());
        instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
        instruction_data.extend_from_slice(8129usize.to_le_bytes().as_ref());

        let instruction = Instruction::new_with_bytes(
            invoke_program_id,
            &instruction_data,
            account_metas.clone(),
        );
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        assert!(result.is_ok());

        let data = bank_client
            .get_account_data(&account_keypair.pubkey())
            .unwrap()
            .unwrap();

        assert_eq!(data.len(), 19480);

        data.iter().enumerate().for_each(|(i, v)| {
            let expected = match i {
                8129 => (i as u8) ^ 0xe5,
                ..=10240 => i as u8,
                16384 => 0xe5,
                _ => 0,
            };

            assert_eq!(*v, expected, "offset:{i} {v:#x} != {expected:#x}");
        });

        // III. do CPI with account with resize to larger and write
        let mut account = AccountSharedData::new(42, 10240, &invoke_program_id);
        let data: Vec<u8> = (0..10240).map(|n| n as u8).collect();
        account.set_data(data);
        bank.store_account(&account_keypair.pubkey(), &account);

        let mut instruction_data = vec![TEST_CALLEE_ACCOUNT_UPDATES, 1, 0];
        instruction_data.extend_from_slice(16384usize.to_le_bytes().as_ref());
        instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
        instruction_data.extend_from_slice(16384usize.to_le_bytes().as_ref());
        // instruction data for inner CPI
        instruction_data.extend_from_slice(&[TEST_CALLEE_ACCOUNT_UPDATES, 0, 0]);
        instruction_data.extend_from_slice(20480usize.to_le_bytes().as_ref());
        instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
        instruction_data.extend_from_slice(16385usize.to_le_bytes().as_ref());

        let instruction = Instruction::new_with_bytes(
            invoke_program_id,
            &instruction_data,
            account_metas.clone(),
        );
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        assert!(result.is_ok());

        let data = bank_client
            .get_account_data(&account_keypair.pubkey())
            .unwrap()
            .unwrap();

        assert_eq!(data.len(), 20480);

        data.iter().enumerate().for_each(|(i, v)| {
            let expected = match i {
                ..=10240 => i as u8,
                16384 | 16385 => 0xe5,
                _ => 0,
            };

            assert_eq!(*v, expected, "offset:{i} {v:#x} != {expected:#x}");
        });

        // IV. do CPI with account with resize to larger and write
        let mut account = AccountSharedData::new(42, 10240, &invoke_program_id);
        let data: Vec<u8> = (0..10240).map(|n| n as u8).collect();
        account.set_data(data);
        bank.store_account(&account_keypair.pubkey(), &account);

        let mut instruction_data = vec![TEST_CALLEE_ACCOUNT_UPDATES, 1, 0];
        instruction_data.extend_from_slice(16384usize.to_le_bytes().as_ref());
        instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
        instruction_data.extend_from_slice(16384usize.to_le_bytes().as_ref());
        // instruction data for inner CPI (2x)
        instruction_data.extend_from_slice(&[TEST_CALLEE_ACCOUNT_UPDATES, 1, 0]);
        instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
        instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
        instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());

        instruction_data.extend_from_slice(&[TEST_CALLEE_ACCOUNT_UPDATES, 1, 0]);
        instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
        instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
        instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
        // instruction data for inner CPI
        instruction_data.extend_from_slice(&[TEST_CALLEE_ACCOUNT_UPDATES, 0, 0]);
        instruction_data.extend_from_slice(20480usize.to_le_bytes().as_ref());
        instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
        instruction_data.extend_from_slice(16385usize.to_le_bytes().as_ref());

        let instruction = Instruction::new_with_bytes(
            invoke_program_id,
            &instruction_data,
            account_metas.clone(),
        );
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
        assert!(result.is_ok());

        let data = bank_client
            .get_account_data(&account_keypair.pubkey())
            .unwrap()
            .unwrap();

        assert_eq!(data.len(), 20480);

        data.iter().enumerate().for_each(|(i, v)| {
            let expected = match i {
                ..=10240 => i as u8,
                16384 | 16385 => 0xe5,
                _ => 0,
            };

            assert_eq!(*v, expected, "offset:{i} {v:#x} != {expected:#x}");
        });

        // V. clone data, modify and CPI
        let mut account = AccountSharedData::new(42, 10240, &invoke_program_id);
        let data: Vec<u8> = (0..10240).map(|n| n as u8).collect();
        account.set_data(data);

        bank.store_account(&account_keypair.pubkey(), &account);

        let mut instruction_data = vec![TEST_CALLEE_ACCOUNT_UPDATES, 1, 1];
        instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
        instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
        instruction_data.extend_from_slice(8190usize.to_le_bytes().as_ref());

        // instruction data for inner CPI
        instruction_data.extend_from_slice(&[TEST_CALLEE_ACCOUNT_UPDATES, 1, 0]);
        instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
        instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
        instruction_data.extend_from_slice(8191usize.to_le_bytes().as_ref());

        let instruction = Instruction::new_with_bytes(
            invoke_program_id,
            &instruction_data,
            account_metas.clone(),
        );
        let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);

        if direct_mapping {
            // changing the data pointer is not permitted
            assert!(result.is_err());
        } else {
            assert!(result.is_ok());

            let data = bank_client
                .get_account_data(&account_keypair.pubkey())
                .unwrap()
                .unwrap();

            assert_eq!(data.len(), 10240);

            data.iter().enumerate().for_each(|(i, v)| {
                let expected = match i {
                    // since the data is was cloned, the write to 8191 was lost
                    8190 => (i as u8) ^ 0xe5,
                    ..=10240 => i as u8,
                    _ => 0,
                };

                assert_eq!(*v, expected, "offset:{i} {v:#x} != {expected:#x}");
            });
        }
    }
}

#[test]
fn test_account_info_in_account() {
    solana_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(100_123_456_789);

    let mut programs = Vec::new();
    #[cfg(feature = "sbf_c")]
    {
        programs.push("invoke");
    }
    #[cfg(feature = "sbf_rust")]
    {
        programs.push("solana_sbf_rust_invoke");
    }

    for program in programs {
        for direct_mapping in [false, true] {
            let mut bank = Bank::new_for_tests(&genesis_config);
            let feature_set = Arc::make_mut(&mut bank.feature_set);
            // by default test banks have all features enabled, so we only need to
            // disable when needed
            if !direct_mapping {
                feature_set.deactivate(&feature_set::bpf_account_data_direct_mapping::id());
            }

            let (bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();
            let mut bank_client = BankClient::new_shared(bank.clone());
            let authority_keypair = Keypair::new();

            let (bank, invoke_program_id) = load_program_of_loader_v4(
                &mut bank_client,
                &bank_forks,
                &mint_keypair,
                &authority_keypair,
                program,
            );

            let account_keypair = Keypair::new();

            let mint_pubkey = mint_keypair.pubkey();

            let account_metas = vec![
                AccountMeta::new(mint_pubkey, true),
                AccountMeta::new(account_keypair.pubkey(), false),
                AccountMeta::new_readonly(invoke_program_id, false),
            ];

            let mut instruction_data = vec![TEST_ACCOUNT_INFO_IN_ACCOUNT];
            instruction_data.extend_from_slice(32usize.to_le_bytes().as_ref());

            let instruction =
                Instruction::new_with_bytes(invoke_program_id, &instruction_data, account_metas);

            let account = AccountSharedData::new(42, 10240, &invoke_program_id);

            bank.store_account(&account_keypair.pubkey(), &account);

            let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);
            if direct_mapping {
                assert!(result.is_err());
            } else {
                assert!(result.is_ok());
            }
        }
    }
}

#[test]
fn test_account_info_rc_in_account() {
    solana_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(100_123_456_789);

    for direct_mapping in [false, true] {
        let mut bank = Bank::new_for_tests(&genesis_config);
        let feature_set = Arc::make_mut(&mut bank.feature_set);
        // by default test banks have all features enabled, so we only need to
        // disable when needed
        if !direct_mapping {
            feature_set.deactivate(&feature_set::bpf_account_data_direct_mapping::id());
        }

        let (bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();
        let mut bank_client = BankClient::new_shared(bank.clone());
        let authority_keypair = Keypair::new();

        let (bank, invoke_program_id) = load_program_of_loader_v4(
            &mut bank_client,
            &bank_forks,
            &mint_keypair,
            &authority_keypair,
            "solana_sbf_rust_invoke",
        );

        let account_keypair = Keypair::new();

        let mint_pubkey = mint_keypair.pubkey();

        let account_metas = vec![
            AccountMeta::new(mint_pubkey, true),
            AccountMeta::new(account_keypair.pubkey(), false),
            AccountMeta::new_readonly(invoke_program_id, false),
        ];

        let instruction_data = vec![TEST_ACCOUNT_INFO_LAMPORTS_RC, 0, 0, 0];

        let instruction = Instruction::new_with_bytes(
            invoke_program_id,
            &instruction_data,
            account_metas.clone(),
        );

        let account = AccountSharedData::new(42, 10240, &invoke_program_id);

        bank.store_account(&account_keypair.pubkey(), &account);

        let message = Message::new(&[instruction], Some(&mint_pubkey));
        let tx = Transaction::new(&[&mint_keypair], message.clone(), bank.last_blockhash());
        let (result, _, logs, _) = process_transaction_and_record_inner(&bank, tx);

        if direct_mapping {
            assert!(
                logs.last().unwrap().ends_with(" failed: Invalid pointer"),
                "{logs:?}"
            );
            assert!(result.is_err());
        } else {
            assert!(result.is_ok(), "{logs:?}");
        }

        let instruction_data = vec![TEST_ACCOUNT_INFO_DATA_RC, 0, 0, 0];

        let instruction =
            Instruction::new_with_bytes(invoke_program_id, &instruction_data, account_metas);

        let account = AccountSharedData::new(42, 10240, &invoke_program_id);

        bank.store_account(&account_keypair.pubkey(), &account);

        let message = Message::new(&[instruction], Some(&mint_pubkey));
        let tx = Transaction::new(&[&mint_keypair], message.clone(), bank.last_blockhash());
        let (result, _, logs, _) = process_transaction_and_record_inner(&bank, tx);

        if direct_mapping {
            assert!(
                logs.last().unwrap().ends_with(" failed: Invalid pointer"),
                "{logs:?}"
            );
            assert!(result.is_err());
        } else {
            assert!(result.is_ok(), "{logs:?}");
        }
    }
}

#[test]
fn test_clone_account_data() {
    // Test cloning account data works as expect with
    solana_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(100_123_456_789);

    let mut bank = Bank::new_for_tests(&genesis_config);
    let feature_set = Arc::make_mut(&mut bank.feature_set);

    feature_set.deactivate(&feature_set::bpf_account_data_direct_mapping::id());

    let (bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();
    let mut bank_client = BankClient::new_shared(bank.clone());
    let authority_keypair = Keypair::new();

    let (_bank, invoke_program_id) = load_program_of_loader_v4(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_invoke",
    );
    let (bank, invoke_program_id2) = load_program_of_loader_v4(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_invoke",
    );

    let account_keypair = Keypair::new();

    let mint_pubkey = mint_keypair.pubkey();

    let account_metas = vec![
        AccountMeta::new(mint_pubkey, true),
        AccountMeta::new(account_keypair.pubkey(), false),
        AccountMeta::new_readonly(invoke_program_id2, false),
        AccountMeta::new_readonly(invoke_program_id, false),
    ];

    // I. clone data and CPI; modify data in callee.
    // Now the original data in the caller is unmodified, and we get a "instruction modified data of an account it does not own"
    // error in the caller
    let mut account = AccountSharedData::new(42, 10240, &invoke_program_id2);
    let data: Vec<u8> = (0..10240).map(|n| n as u8).collect();
    account.set_data(data);

    bank.store_account(&account_keypair.pubkey(), &account);

    let mut instruction_data = vec![TEST_CALLEE_ACCOUNT_UPDATES, 1, 1];
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());

    // instruction data for inner CPI: modify account
    instruction_data.extend_from_slice(&[TEST_CALLEE_ACCOUNT_UPDATES, 0, 0]);
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(8190usize.to_le_bytes().as_ref());

    let instruction =
        Instruction::new_with_bytes(invoke_program_id, &instruction_data, account_metas.clone());

    let message = Message::new(&[instruction], Some(&mint_pubkey));
    let tx = Transaction::new(&[&mint_keypair], message.clone(), bank.last_blockhash());
    let (result, _, logs, _) = process_transaction_and_record_inner(&bank, tx);
    assert!(result.is_err(), "{result:?}");
    let error = format!("Program {invoke_program_id} failed: instruction modified data of an account it does not own");
    assert!(logs.iter().any(|log| log.contains(&error)), "{logs:?}");

    // II. clone data, modify and then CPI
    // The deserialize checks should verify that we're not allowed to modify an account we don't own, even though
    // we have only modified a copy of the data. Fails in caller
    let mut account = AccountSharedData::new(42, 10240, &invoke_program_id2);
    let data: Vec<u8> = (0..10240).map(|n| n as u8).collect();
    account.set_data(data);

    bank.store_account(&account_keypair.pubkey(), &account);

    let mut instruction_data = vec![TEST_CALLEE_ACCOUNT_UPDATES, 1, 1];
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(8190usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());

    // instruction data for inner CPI
    instruction_data.extend_from_slice(&[TEST_CALLEE_ACCOUNT_UPDATES, 0, 0]);
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());

    let instruction =
        Instruction::new_with_bytes(invoke_program_id, &instruction_data, account_metas.clone());

    let message = Message::new(&[instruction], Some(&mint_pubkey));
    let tx = Transaction::new(&[&mint_keypair], message.clone(), bank.last_blockhash());
    let (result, _, logs, _) = process_transaction_and_record_inner(&bank, tx);
    assert!(result.is_err(), "{result:?}");
    let error = format!("Program {invoke_program_id} failed: instruction modified data of an account it does not own");
    assert!(logs.iter().any(|log| log.contains(&error)), "{logs:?}");

    // II. Clone data, call, modifiy in callee and then make the same change in the caller - transaction succeeds
    // Note the caller needs to modify the original account data, not the copy
    let mut account = AccountSharedData::new(42, 10240, &invoke_program_id2);
    let data: Vec<u8> = (0..10240).map(|n| n as u8).collect();
    account.set_data(data);

    bank.store_account(&account_keypair.pubkey(), &account);

    let mut instruction_data = vec![TEST_CALLEE_ACCOUNT_UPDATES, 1, 1];
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(8190usize.to_le_bytes().as_ref());

    // instruction data for inner CPI
    instruction_data.extend_from_slice(&[TEST_CALLEE_ACCOUNT_UPDATES, 0, 0]);
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(8190usize.to_le_bytes().as_ref());

    let instruction =
        Instruction::new_with_bytes(invoke_program_id, &instruction_data, account_metas.clone());
    let result = bank_client.send_and_confirm_instruction(&mint_keypair, instruction);

    // works because the account is exactly the same in caller as callee
    assert!(result.is_ok(), "{result:?}");
}

#[test]
fn test_stack_heap_zeroed() {
    solana_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(100_123_456_789);

    let bank = Bank::new_for_tests(&genesis_config);

    let (bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();
    let mut bank_client = BankClient::new_shared(bank);
    let authority_keypair = Keypair::new();

    let (bank, invoke_program_id) = load_program_of_loader_v4(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_invoke",
    );

    let account_keypair = Keypair::new();
    let mint_pubkey = mint_keypair.pubkey();
    let account_metas = vec![
        AccountMeta::new(mint_pubkey, true),
        AccountMeta::new(account_keypair.pubkey(), false),
        AccountMeta::new_readonly(invoke_program_id, false),
    ];

    // Check multiple heap sizes. It's generally a good idea, and also it's needed to ensure that
    // pooled heap and stack values are reused - and therefore zeroed - across executions.
    for heap_len in [32usize * 1024, 64 * 1024, 128 * 1024, 256 * 1024] {
        // TEST_STACK_HEAP_ZEROED will recursively check that stack and heap are zeroed until it
        // reaches max CPI invoke depth. We make it fail at max depth so we're sure that there's no
        // legit way to access non-zeroed stack and heap regions.
        let mut instruction_data = vec![TEST_STACK_HEAP_ZEROED];
        instruction_data.extend_from_slice(&heap_len.to_le_bytes());

        let instruction = Instruction::new_with_bytes(
            invoke_program_id,
            &instruction_data,
            account_metas.clone(),
        );

        let message = Message::new(
            &[
                ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
                ComputeBudgetInstruction::request_heap_frame(heap_len as u32),
                instruction,
            ],
            Some(&mint_pubkey),
        );
        let tx = Transaction::new(&[&mint_keypair], message.clone(), bank.last_blockhash());
        let (result, _, logs, _) = process_transaction_and_record_inner(&bank, tx);
        assert!(result.is_err(), "{result:?}");
        assert!(
            logs.iter()
                .any(|log| log.contains("Cross-program invocation call depth too deep")),
            "{logs:?}"
        );
    }
}

#[test]
fn test_function_call_args() {
    // This function tests edge compiler edge cases when calling functions with more than five
    // arguments and passing by value arguments with more than 16 bytes.
    solana_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(100_123_456_789);

    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let mut bank_client = BankClient::new_shared(bank);
    let authority_keypair = Keypair::new();

    let (bank, program_id) = load_program_of_loader_v4(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_call_args",
    );

    #[derive(BorshSerialize, BorshDeserialize, PartialEq, Eq, Debug)]
    struct Test128 {
        a: u128,
        b: u128,
    }

    #[derive(BorshSerialize)]
    struct InputData {
        test_128: Test128,
        arg1: i64,
        arg2: i64,
        arg3: i64,
        arg4: i64,
        arg5: i64,
        arg6: i64,
        arg7: i64,
        arg8: i64,
    }

    #[derive(BorshDeserialize)]
    struct OutputData {
        res_128: u128,
        res_256: Test128,
        many_args_1: i64,
        many_args_2: i64,
    }

    let input_data = InputData {
        test_128: Test128 {
            a: rand::random::<u128>(),
            b: rand::random::<u128>(),
        },
        arg1: rand::random::<i64>(),
        arg2: rand::random::<i64>(),
        arg3: rand::random::<i64>(),
        arg4: rand::random::<i64>(),
        arg5: rand::random::<i64>(),
        arg6: rand::random::<i64>(),
        arg7: rand::random::<i64>(),
        arg8: rand::random::<i64>(),
    };

    let instruction_data = to_vec(&input_data).unwrap();
    let account_metas = vec![
        AccountMeta::new(mint_keypair.pubkey(), true),
        AccountMeta::new(Keypair::new().pubkey(), false),
    ];

    let instruction = Instruction::new_with_bytes(program_id, &instruction_data, account_metas);
    let message = Message::new(&[instruction], Some(&mint_keypair.pubkey()));

    let tx = Transaction::new(&[&mint_keypair], message.clone(), bank.last_blockhash());

    let txs = vec![tx];
    let tx_batch = bank.prepare_batch_for_tests(txs);
    let result = bank
        .load_execute_and_commit_transactions(
            &tx_batch,
            MAX_PROCESSING_AGE,
            ExecutionRecordingConfig {
                enable_cpi_recording: false,
                enable_log_recording: false,
                enable_return_data_recording: true,
                enable_transaction_balance_recording: false,
            },
            &mut ExecuteTimings::default(),
            None,
        )
        .0;

    fn verify_many_args(input: &InputData) -> i64 {
        let a = input
            .arg1
            .overflowing_add(input.arg2)
            .0
            .overflowing_sub(input.arg3)
            .0
            .overflowing_add(input.arg4)
            .0
            .overflowing_sub(input.arg5)
            .0;
        (a % input.arg6)
            .overflowing_sub(input.arg7)
            .0
            .overflowing_add(input.arg8)
            .0
    }

    let return_data = &result[0]
        .as_ref()
        .unwrap()
        .return_data
        .as_ref()
        .unwrap()
        .data;
    let decoded: OutputData = from_slice::<OutputData>(return_data).unwrap();
    assert_eq!(
        decoded.res_128,
        input_data.test_128.a % input_data.test_128.b
    );
    assert_eq!(
        decoded.res_256,
        Test128 {
            a: input_data
                .test_128
                .a
                .overflowing_add(input_data.test_128.b)
                .0,
            b: input_data
                .test_128
                .a
                .overflowing_sub(input_data.test_128.b)
                .0
        }
    );
    assert_eq!(decoded.many_args_1, verify_many_args(&input_data));
    assert_eq!(decoded.many_args_2, verify_many_args(&input_data));
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_mem_syscalls_overlap_account_begin_or_end() {
    solana_logger::setup();

    for direct_mapping in [false, true] {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(100_123_456_789);

        let mut bank = Bank::new_for_tests(&genesis_config);
        let mut feature_set = FeatureSet::all_enabled();
        if !direct_mapping {
            feature_set.deactivate(&feature_set::bpf_account_data_direct_mapping::id());
        }

        let account_keypair = Keypair::new();

        bank.feature_set = Arc::new(feature_set);
        let (bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();
        let mut bank_client = BankClient::new_shared(bank);
        let authority_keypair = Keypair::new();

        let (bank, loader_v4_program_id) = load_program_of_loader_v4(
            &mut bank_client,
            &bank_forks,
            &mint_keypair,
            &authority_keypair,
            "solana_sbf_rust_account_mem",
        );

        let deprecated_program_id = create_program(
            &bank,
            &bpf_loader_deprecated::id(),
            "solana_sbf_rust_account_mem_deprecated",
        );

        let mint_pubkey = mint_keypair.pubkey();

        for deprecated in [false, true] {
            let program_id = if deprecated {
                deprecated_program_id
            } else {
                loader_v4_program_id
            };

            let account_metas = vec![
                AccountMeta::new(mint_pubkey, true),
                AccountMeta::new_readonly(program_id, false),
                AccountMeta::new(account_keypair.pubkey(), false),
            ];

            let account = AccountSharedData::new(42, 1024, &program_id);
            bank.store_account(&account_keypair.pubkey(), &account);

            for instr in 0..=15 {
                println!("Testing deprecated:{deprecated} direct_mapping:{direct_mapping} instruction:{instr}");
                let instruction =
                    Instruction::new_with_bytes(program_id, &[instr], account_metas.clone());

                let message = Message::new(&[instruction], Some(&mint_pubkey));
                let tx = Transaction::new(&[&mint_keypair], message.clone(), bank.last_blockhash());
                let (result, _, logs, _) = process_transaction_and_record_inner(&bank, tx);
                let last_line = logs.last().unwrap();

                if direct_mapping {
                    assert!(last_line.contains(" failed: Access violation"), "{logs:?}");
                } else {
                    assert!(result.is_ok(), "{logs:?}");
                }
            }

            let account = AccountSharedData::new(42, 0, &program_id);
            bank.store_account(&account_keypair.pubkey(), &account);

            for instr in 0..=15 {
                println!("Testing deprecated:{deprecated} direct_mapping:{direct_mapping} instruction:{instr} zero-length account");
                let instruction =
                    Instruction::new_with_bytes(program_id, &[instr, 0], account_metas.clone());

                let message = Message::new(&[instruction], Some(&mint_pubkey));
                let tx = Transaction::new(&[&mint_keypair], message.clone(), bank.last_blockhash());
                let (result, _, logs, _) = process_transaction_and_record_inner(&bank, tx);
                let last_line = logs.last().unwrap();

                if direct_mapping && (!deprecated || instr < 8) {
                    assert!(
                        last_line.contains(" failed: account data too small")
                            || last_line.contains(" failed: Failed to reallocate account data")
                            || last_line.contains(" failed: Access violation"),
                        "{logs:?}",
                    );
                } else {
                    // direct_mapping && deprecated && instr >= 8 succeeds with zero-length accounts
                    // because there is no MemoryRegion for the account,
                    // so there can be no error when leaving that non-existent region.
                    assert!(result.is_ok(), "{logs:?}");
                }
            }
        }
    }
}
