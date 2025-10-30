#![cfg(feature = "shuttle-test")]

use {
    crate::mock_bank::{create_custom_loader, deploy_program, register_builtins, MockForkGraph},
    assert_matches::assert_matches,
    mock_bank::MockBankCallback,
    shuttle::{
        sync::{Arc, RwLock},
        thread, Runner,
    },
    solana_account::{AccountSharedData, ReadableAccount, WritableAccount},
    solana_instruction::{AccountMeta, Instruction},
    solana_program_runtime::{
        execution_budget::SVMTransactionExecutionAndFeeBudgetLimits,
        loaded_programs::{ProgramCacheEntryType, ProgramCacheForTxBatch},
    },
    solana_pubkey::Pubkey,
    solana_svm::{
        account_loader::{AccountLoader, CheckedTransactionDetails, TransactionCheckResult},
        transaction_processing_result::{
            ProcessedTransaction, TransactionProcessingResultExtensions,
        },
        transaction_processor::{
            ExecutionRecordingConfig, TransactionBatchProcessor, TransactionProcessingConfig,
            TransactionProcessingEnvironment,
        },
    },
    solana_svm_feature_set::SVMFeatureSet,
    solana_svm_timings::ExecuteTimings,
    solana_transaction::{sanitized::SanitizedTransaction, Transaction},
    std::collections::HashSet,
};

mod mock_bank;

const MAX_ITERATIONS: usize = 10_000;

fn program_cache_execution(threads: usize) {
    let mut mock_bank = MockBankCallback::default();
    let fork_graph = Arc::new(RwLock::new(MockForkGraph {}));
    let batch_processor =
        TransactionBatchProcessor::new(5, 5, Arc::downgrade(&fork_graph), None, None);

    let programs = vec![
        deploy_program("hello-solana".to_string(), 0, &mut mock_bank),
        deploy_program("simple-transfer".to_string(), 0, &mut mock_bank),
        deploy_program("clock-sysvar".to_string(), 0, &mut mock_bank),
    ];

    let account_maps: HashSet<Pubkey> = programs.iter().copied().collect();

    let ths: Vec<_> = (0..threads)
        .map(|_| {
            let local_bank = mock_bank.clone();
            let processor = TransactionBatchProcessor::new_from(
                &batch_processor,
                batch_processor.slot,
                batch_processor.epoch,
            );
            let maps = account_maps.clone();
            let programs = programs.clone();
            thread::spawn(move || {
                let feature_set = SVMFeatureSet::all_enabled();
                let account_loader = AccountLoader::new_with_loaded_accounts_capacity(
                    None,
                    &local_bank,
                    &feature_set,
                    0,
                );
                let mut result = ProgramCacheForTxBatch::new(processor.slot);
                let program_runtime_environments_for_execution =
                    processor.get_environments_for_epoch(processor.epoch);
                processor.replenish_program_cache(
                    &account_loader,
                    &maps,
                    &program_runtime_environments_for_execution,
                    &mut result,
                    &mut ExecuteTimings::default(),
                    false,
                    true,
                    true,
                );
                for key in &programs {
                    let cache_entry = result.find(key);
                    assert!(matches!(
                        cache_entry.unwrap().program,
                        ProgramCacheEntryType::Loaded(_)
                    ));
                }
            })
        })
        .collect();

    for th in ths {
        th.join().unwrap();
    }
}

// Shuttle has its own internal scheduler and the following tests change the way it operates to
// increase the efficiency in finding problems in the program cache's concurrent code.

// This test leverages the probabilistic concurrency testing algorithm
// (https://www.microsoft.com/en-us/research/wp-content/uploads/2016/02/asplos277-pct.pdf).
// It bounds the numbers of preemptions to explore (five in this test) for the four
// threads we use. We run it for 300 iterations.
#[test]
fn test_program_cache_with_probabilistic_scheduler() {
    shuttle::check_pct(
        move || {
            program_cache_execution(4);
        },
        MAX_ITERATIONS,
        5,
    );
}

// In this case, the scheduler is random and may preempt threads at any point and any time.
#[test]
fn test_program_cache_with_random_scheduler() {
    shuttle::check_random(move || program_cache_execution(4), MAX_ITERATIONS);
}

// This test explores all the possible thread scheduling patterns that might affect the program
// cache. There is a limitation to run only 500 iterations to avoid consuming too much CI time.
#[test]
fn test_program_cache_with_exhaustive_scheduler() {
    // The DFS (shuttle::check_dfs) test is only complete when we do not generate random
    // values in a thread.
    // Since this is not the case for the execution of jitted program, we can still run the test
    // but with decreased accuracy.
    let scheduler = shuttle::scheduler::DfsScheduler::new(Some(MAX_ITERATIONS), true);
    let runner = Runner::new(scheduler, Default::default());
    runner.run(move || program_cache_execution(4));
}

// This test executes multiple transactions in parallel where all read from the same data account,
// but write to different accounts. Given that there are no locks in this case, SVM must behave
// correctly.
fn svm_concurrent() {
    let mock_bank = Arc::new(MockBankCallback::default());
    let fork_graph = Arc::new(RwLock::new(MockForkGraph {}));
    let batch_processor = Arc::new(TransactionBatchProcessor::new(
        5,
        2,
        Arc::downgrade(&fork_graph),
        Some(Arc::new(create_custom_loader())),
        None, // We are not using program runtime v2.
    ));

    mock_bank.configure_sysvars();
    batch_processor.fill_missing_sysvar_cache_entries(&*mock_bank);
    register_builtins(&mock_bank, &batch_processor, false);

    let program_id = deploy_program("transfer-from-account".to_string(), 0, &mock_bank);

    const THREADS: usize = 4;
    const TRANSACTIONS_PER_THREAD: usize = 3;
    const AMOUNT: u64 = 50;
    const CAPACITY: usize = THREADS * TRANSACTIONS_PER_THREAD;
    const BALANCE: u64 = 500000;

    let mut transactions = vec![Vec::new(); THREADS];
    let mut check_data = vec![Vec::new(); THREADS];
    let read_account = Pubkey::new_unique();
    let mut account_data = AccountSharedData::default();
    account_data.set_data(AMOUNT.to_le_bytes().to_vec());
    account_data.set_rent_epoch(u64::MAX);
    account_data.set_lamports(1);
    mock_bank
        .account_shared_data
        .write()
        .unwrap()
        .insert(read_account, account_data);

    #[derive(Clone)]
    struct CheckTxData {
        sender: Pubkey,
        recipient: Pubkey,
        fee_payer: Pubkey,
    }

    for idx in 0..CAPACITY {
        let sender = Pubkey::new_unique();
        let recipient = Pubkey::new_unique();
        let fee_payer = Pubkey::new_unique();
        let system_account = Pubkey::from([0u8; 32]);

        let mut account_data = AccountSharedData::default();
        account_data.set_lamports(BALANCE);

        {
            let shared_data = &mut mock_bank.account_shared_data.write().unwrap();
            shared_data.insert(sender, account_data.clone());
            shared_data.insert(recipient, account_data.clone());
            shared_data.insert(fee_payer, account_data);
        }

        let accounts = vec![
            AccountMeta {
                pubkey: sender,
                is_signer: true,
                is_writable: true,
            },
            AccountMeta {
                pubkey: recipient,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: read_account,
                is_signer: false,
                is_writable: false,
            },
            AccountMeta {
                pubkey: system_account,
                is_signer: false,
                is_writable: false,
            },
        ];

        let instruction = Instruction::new_with_bytes(program_id, &[0], accounts);
        let legacy_transaction = Transaction::new_with_payer(&[instruction], Some(&fee_payer));

        let sanitized_transaction =
            SanitizedTransaction::try_from_legacy_transaction(legacy_transaction, &HashSet::new());
        transactions[idx % THREADS].push(sanitized_transaction.unwrap());
        check_data[idx % THREADS].push(CheckTxData {
            fee_payer,
            recipient,
            sender,
        });
    }

    let ths: Vec<_> = (0..THREADS)
        .map(|idx| {
            let local_batch = batch_processor.clone();
            let local_bank = mock_bank.clone();
            let th_txs = std::mem::take(&mut transactions[idx]);
            let check_results = th_txs
                .iter()
                .map(|tx| {
                    Ok(CheckedTransactionDetails::new(
                        None,
                        Ok(SVMTransactionExecutionAndFeeBudgetLimits::with_fee(
                            MockBankCallback::calculate_fee_details(tx, 0),
                        )),
                    )) as TransactionCheckResult
                })
                .collect();
            let processing_config = TransactionProcessingConfig {
                recording_config: ExecutionRecordingConfig {
                    enable_log_recording: true,
                    enable_return_data_recording: false,
                    enable_cpi_recording: false,
                    enable_transaction_balance_recording: false,
                },
                ..Default::default()
            };
            let check_tx_data = std::mem::take(&mut check_data[idx]);

            thread::spawn(move || {
                let result = local_batch.load_and_execute_sanitized_transactions(
                    &*local_bank,
                    &th_txs,
                    check_results,
                    &TransactionProcessingEnvironment {
                        program_runtime_environments_for_execution: local_batch
                            .environments
                            .clone(),
                        ..TransactionProcessingEnvironment::default()
                    },
                    &processing_config,
                );

                for (idx, processing_result) in result.processing_results.iter().enumerate() {
                    assert!(processing_result.was_processed());
                    let processed_tx = processing_result.processed_transaction().unwrap();
                    assert_matches!(processed_tx, &ProcessedTransaction::Executed(_));
                    let executed_tx = processed_tx.executed_transaction().unwrap();
                    let inserted_accounts = &check_tx_data[idx];
                    for (key, account_data) in &executed_tx.loaded_transaction.accounts {
                        if *key == inserted_accounts.fee_payer {
                            assert_eq!(account_data.lamports(), BALANCE - 10000);
                        } else if *key == inserted_accounts.sender {
                            assert_eq!(account_data.lamports(), BALANCE - AMOUNT);
                        } else if *key == inserted_accounts.recipient {
                            assert_eq!(account_data.lamports(), BALANCE + AMOUNT);
                        }
                    }
                }
            })
        })
        .collect();

    for th in ths {
        th.join().unwrap();
    }
}

#[test]
fn test_svm_with_probabilistic_scheduler() {
    shuttle::check_pct(
        move || {
            svm_concurrent();
        },
        MAX_ITERATIONS,
        5,
    );
}
