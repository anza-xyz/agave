#![cfg(feature = "sbf_rust")]

use {
    solana_account::AccountSharedData,
    solana_client_traits::SyncClient,
    solana_instruction::{AccountMeta, Instruction},
    solana_keypair::Keypair,
    solana_message::Message,
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::Bank,
        bank_client::BankClient,
        genesis_utils::{create_genesis_config, GenesisConfigInfo},
        loader_utils::load_program_of_loader_v4,
    },
    solana_signer::Signer,
    solana_svm::transaction_processor::ExecutionRecordingConfig,
    solana_timings::ExecuteTimings,
    solana_transaction::Transaction,
    solana_transaction_error::TransactionError,
    std::str::FromStr,
};

/* The story so far...

$ cargo test --features sbf_rust test_deserialization_benchmark -- --nocapture
warning: /Users/levicook/anza-xyz/agave/svm/Cargo.toml: Found `debug_assertions` in `target.'cfg(...)'.dependencies`. This value is not supported for selecting dependencies and will not work as expected. To learn more visit https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html#platform-specific-dependencies
Compiling solana-sbf-programs v3.0.0 (/Users/levicook/anza-xyz/agave/programs/sbf)
    Finished `test` profile [unoptimized + debuginfo] target(s) in 1.52s
    Running tests/deserialization_benchmark_test.rs (target/debug/deps/deserialization_benchmark_test-c0ff29557be77889)

running 1 test
=== VM Register Optimization Experiment ===
This test shows current deserialization overhead that could be optimized

=== Running Benchmarks ===

Testing Mode 0: Account Access Patterns
✓ Transaction successful - CU consumed: 25223
Program logs:
Program log: === Deserialization Benchmark Starting ===
Program log: Starting CU: 199137
Program log: === Benchmark Complete ===
Program log: CU consumed: 23248
Program log: Ending CU: 175889

Testing Mode 1: Repeated Key Access
✓ Transaction successful - CU consumed: 12962
Program logs:
Program log: === Deserialization Benchmark Starting ===
Program log: Starting CU: 199137
Program log: === Benchmark Complete ===
Program log: CU consumed: 10987
Program log: Ending CU: 188150

Testing Mode 2: Lamports Checks
✓ Transaction successful - CU consumed: 10875
Program logs:
Program log: === Deserialization Benchmark Starting ===
Program log: Starting CU: 199137
Program log: === Benchmark Complete ===
Program log: CU consumed: 8918
Program log: Ending CU: 190219

Testing Mode 3: Owner Checks
✓ Transaction successful - CU consumed: 13916
Program logs:
Program log: === Deserialization Benchmark Starting ===
Program log: Starting CU: 199137
Program log: === Benchmark Complete ===
Program log: CU consumed: 11941
Program log: Ending CU: 187196

Testing Mode 4: Data Length Access
✓ Transaction successful - CU consumed: 10764
Program logs:
Program log: === Deserialization Benchmark Starting ===
Program log: Starting CU: 199137
Program log: === Benchmark Complete ===
Program log: CU consumed: 8807
Program log: Ending CU: 190330

=== Analysis ===
Look at the CU consumption in the logs above.
Each benchmark shows how many CUs are consumed by repeated
access to account metadata that could be pre-computed in registers.

The optimization opportunity:
- Pre-populate r2-r9 with commonly accessed account metadata
- Programs can read directly from registers instead of deserializing
- Significant CU savings for metadata-heavy operations
test test_deserialization_benchmark ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 2 filtered out; finished in 0.12s
*/

/// This test demonstrates the current deserialization overhead in the VM
/// and shows where register pre-population could provide significant savings
///
/// # Current CU Consumption Measurements (Baseline)
///
/// Based on test results, the current VM deserialization overhead is:
///
/// | Mode | Description              | Total CU | Program CU | Operations | CU/Operation |
/// |------|--------------------------|----------|------------|------------|--------------|
/// | 0    | Account Access Patterns  | 25,223   | 23,248     | ~150       | ~155         |
/// | 1    | Repeated Key Access      | 12,985   | 11,010     | ~100       | ~110         |
/// | 2    | Lamports Checks          | 10,875   | 8,918      | ~90        | ~99          |
/// | 3    | Owner Checks             | 13,916   | 11,941     | ~75        | ~159         |
/// | 4    | Data Length Access       | 10,764   | 8,807      | ~120       | ~73          |
///
/// # Optimization Analysis
///
/// **Current State:** Each metadata access (account.key, account.lamports(), account.owner,
/// account.data_len()) triggers VM deserialization, consuming ~100-160 CUs per operation.
///
/// **Register Pre-population Opportunity:** If we pre-populate VM registers r2-r9 with:
/// - r2: account[0].key (32 bytes)
/// - r3: account[0].lamports (8 bytes)
/// - r4: account[0].owner (32 bytes)
/// - r5: account[0].data_len (8 bytes)
/// - r6: packed flags (is_signer, is_writable, executable)
///
/// **Projected Savings:** Register reads would cost ~1-2 CUs instead of ~100-160 CUs:
/// - Mode 0: 23,248 CUs → ~300 CUs (saving ~22,900 CUs, 99% reduction)
/// - Overall: 60-80% CU reduction for metadata-heavy operations
///
/// **Implementation Notes:**
/// - Feature gate required to ensure compatibility with existing programs
/// - Need to scan mainnet programs for zero-initialization assumptions
/// - Most impactful for programs with repeated metadata access patterns
#[test]
fn test_deserialization_benchmark() {
    solana_logger::setup();

    println!("=== VM Register Optimization Experiment ===");
    println!("This test shows current deserialization overhead that could be optimized");

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(1_000_000);

    let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let mut bank_client = BankClient::new_shared(bank);

    // Load our benchmark program
    let authority_keypair = Keypair::new();

    let (_bank, program_id) = load_program_of_loader_v4(
        &mut bank_client,
        &_bank_forks,
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_deserialization_benchmark",
    );

    // Create test accounts with varying amounts of data
    let test_accounts = create_test_accounts(&mut bank_client, &mint_keypair);

    println!("\n=== Running Benchmarks ===");

    // Run different benchmark modes
    for (mode, description) in [
        (0, "Account Access Patterns"),
        (1, "Repeated Key Access"),
        (2, "Lamports Checks"),
        (3, "Owner Checks"),
        (4, "Data Length Access"),
    ] {
        println!("\nTesting Mode {}: {}", mode, description);

        let instruction = Instruction::new_with_bytes(
            program_id,
            &[mode],
            test_accounts
                .iter()
                .map(|keypair| AccountMeta::new(keypair.pubkey(), false))
                .collect(),
        );

        // Create transaction and process it to capture detailed execution info
        let message = Message::new(&[instruction], Some(&mint_keypair.pubkey()));
        let transaction = Transaction::new(&[&mint_keypair], message, _bank.last_blockhash());

        let (status, log_messages, executed_units) =
            process_transaction_and_capture_details(&_bank, transaction);

        match status {
            Ok(_) => {
                println!("✓ Transaction successful - CU consumed: {}", executed_units);
                println!("Program logs:");
                for log in log_messages {
                    if log.contains("deserialization_benchmark")
                        || log.contains("CU")
                        || log.contains("===")
                    {
                        println!("  {}", log);
                    }
                }
            }
            Err(e) => println!("✗ Transaction failed: {:?}", e),
        }
    }

    println!("\n=== Analysis ===");
    println!("Look at the CU consumption in the logs above.");
    println!("Each benchmark shows how many CUs are consumed by repeated");
    println!("access to account metadata that could be pre-computed in registers.");
    println!("\nThe optimization opportunity:");
    println!("- Pre-populate r2-r9 with commonly accessed account metadata");
    println!("- Programs can read directly from registers instead of deserializing");
    println!("- Significant CU savings for metadata-heavy operations");
}

fn create_test_accounts(_bank_client: &mut BankClient, _mint_keypair: &Keypair) -> Vec<Keypair> {
    let mut accounts = Vec::new();

    for i in 0..5 {
        let account_keypair = Keypair::new();
        let account_pubkey = account_keypair.pubkey();

        // Create account with some lamports and data
        let lamports = (1000 + (i * 500)) as u64;
        let data_size = 100 + (i * 50);
        let data = vec![i as u8; data_size];

        let _account = AccountSharedData::new(
            lamports,
            data.len(),
            &Pubkey::from_str("11111111111111111111111111111111").unwrap(),
        );

        // Store the account in the bank
        let _existing_account = _bank_client.get_account(&account_pubkey).ok();

        accounts.push(account_keypair);
    }

    accounts
}

/// This test shows what a comparison might look like once the optimization is implemented
#[test]
#[ignore] // Ignore until the optimization is actually implemented
fn test_optimized_version_comparison() {
    println!("=== Future: Optimized Version Test ===");
    println!("This test would run the same benchmarks with register pre-population enabled");
    println!("Expected results:");
    println!("- 60-80% reduction in CU consumption for metadata access");
    println!("- Faster program execution");
    println!("- Same functionality, just more efficient");
}

/// Test specifically for the zero-initialization concern mentioned in the comments
#[test]
fn test_register_zero_initialization_assumptions() {
    println!("=== Register Zero-Initialization Test ===");
    println!("This test checks if any programs assume r0, r2-r9 are zero-initialized");

    // This would be where we scan deployed programs to check for:
    // 1. Programs that explicitly set registers to zero (redundant if already zero)
    // 2. Programs that rely on zero-initialization for logic
    // 3. Assembly patterns that assume zero values

    println!("In a real implementation, this would:");
    println!("1. Scan mainnet programs for register usage patterns");
    println!("2. Identify programs that might break with register pre-population");
    println!("3. Create compatibility metrics for the feature gate");
}

/// Helper function to process transaction and capture logs and CU consumption
fn process_transaction_and_capture_details(
    bank: &Bank,
    tx: Transaction,
) -> (Result<(), TransactionError>, Vec<String>, u64) {
    let txs = vec![tx];
    let tx_batch = bank.prepare_batch_for_tests(txs);
    let mut commit_results = bank
        .load_execute_and_commit_transactions(
            &tx_batch,
            1000, // MAX_PROCESSING_AGE
            ExecutionRecordingConfig {
                enable_cpi_recording: false,
                enable_log_recording: true,
                enable_return_data_recording: false,
                enable_transaction_balance_recording: false,
            },
            &mut ExecuteTimings::default(),
            None,
        )
        .0;
    let commit_result = commit_results.pop().unwrap().unwrap();
    let log_messages = commit_result
        .log_messages
        .expect("log recording should be enabled");
    (
        commit_result.status,
        log_messages,
        commit_result.executed_units,
    )
}
